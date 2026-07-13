//! Integration test: `SidecarManager` drives a modality-neutral v2 sidecar
//! fixture and surfaces a final InsertText event.

use std::path::PathBuf;
use std::time::Duration;

use synaps_cli::sidecar::manager::{SidecarLifecycleEvent, SidecarManager};
use synaps_cli::sidecar::protocol::{InsertTextMode, SIDECAR_PROTOCOL_VERSION};

fn locate_sidecar() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mock_sidecar_v2.py")
}

#[tokio::test]
async fn manager_drives_sidecar_insert_text_end_to_end() {
    let bin = locate_sidecar();
    assert!(bin.is_file(), "mock sidecar fixture missing at {}", bin.display());

    let mut manager = SidecarManager::spawn(
        &bin,
        &[],
        serde_json::json!({ "protocol_version": SIDECAR_PROTOCOL_VERSION }),
    )
    .await
    .expect("manager spawn should succeed");

    manager.press().await.expect("press should send");
    manager.release().await.expect("release should send");

    let mut got_active = false;
    let mut got_insert_text: Option<String> = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        let remaining = deadline - tokio::time::Instant::now();
        let timed = tokio::time::timeout(remaining, manager.next_event()).await;
        let Ok(Some(event)) = timed else { break };
        match event {
            SidecarLifecycleEvent::StateChanged { state, .. } if state == "active" => {
                got_active = true;
            }
            SidecarLifecycleEvent::InsertText {
                text,
                mode: InsertTextMode::Final,
            } => {
                got_insert_text = Some(text);
                break;
            }
            SidecarLifecycleEvent::Error(err) => panic!("unexpected sidecar error: {err}"),
            _ => {}
        }
    }

    assert!(got_active, "expected active state event");
    assert_eq!(
        got_insert_text.as_deref(),
        Some("hello from sidecar"),
        "expected final InsertText event"
    );

    manager.shutdown().await.expect("graceful shutdown");
}
