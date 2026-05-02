//! Integration test: `VoiceManager` drives a real sidecar binary in
//! `--mock-transcript` mode and surfaces a `FinalTranscript` event.
//!
//! Skipped at runtime if the local-voice-plugin binary isn't built.

use std::path::PathBuf;
use std::time::Duration;

use synaps_cli::voice::manager::{VoiceManager, VoiceManagerEvent};
use synaps_cli::voice::protocol::{SidecarConfig, VoiceSidecarMode, VOICE_SIDECAR_PROTOCOL_VERSION};

fn locate_sidecar() -> Option<PathBuf> {
    // Prefer an explicit env var (CI / Nix builds).
    if let Ok(p) = std::env::var("SYNAPS_VOICE_PLUGIN_BIN") {
        let path = PathBuf::from(p);
        if path.is_file() {
            return Some(path);
        }
    }
    // Fall back to the sibling worktree where the plugin lives.
    let candidates = [
        "/home/jr/Projects/Maha-Media/.worktrees/synaps-skills-local-voice-plugin/local-voice-plugin/target/release/synaps-voice-plugin",
        "/home/jr/Projects/Maha-Media/.worktrees/synaps-skills-local-voice-plugin/local-voice-plugin/target/debug/synaps-voice-plugin",
    ];
    candidates.iter().map(PathBuf::from).find(|p| p.is_file())
}

#[tokio::test]
async fn manager_drives_sidecar_mock_transcript_end_to_end() {
    let Some(bin) = locate_sidecar() else {
        eprintln!("skipping: synaps-voice-plugin binary not found");
        return;
    };

    let mut manager = VoiceManager::spawn(
        &bin,
        &["--mock-transcript".into(), "hello from the sidecar".into()],
        SidecarConfig {
            mode: VoiceSidecarMode::Dictation,
            language: None,
            protocol_version: VOICE_SIDECAR_PROTOCOL_VERSION,
        },
    )
    .await
    .expect("manager spawn should succeed");

    // Drive the toggle: press then release.
    manager.press().await.expect("press should send");
    manager.release().await.expect("release should send");

    // Collect events with a generous timeout.
    let mut got_listening = false;
    let mut got_transcript: Option<String> = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        let remaining = deadline - tokio::time::Instant::now();
        let timed = tokio::time::timeout(remaining, manager.next_event()).await;
        let Ok(Some(event)) = timed else { break };
        match event {
            VoiceManagerEvent::ListeningStarted => got_listening = true,
            VoiceManagerEvent::FinalTranscript(text) => {
                got_transcript = Some(text);
                break;
            }
            VoiceManagerEvent::Error(err) => panic!("unexpected sidecar error: {err}"),
            _ => {}
        }
    }

    assert!(got_listening, "expected ListeningStarted event");
    assert_eq!(
        got_transcript.as_deref(),
        Some("hello from the sidecar"),
        "expected mock transcript to surface as a FinalTranscript event"
    );

    manager.shutdown().await.expect("graceful shutdown");
}
