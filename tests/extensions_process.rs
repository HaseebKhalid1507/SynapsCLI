use synaps_cli::extensions::hooks::events::{HookEvent, HookResult};
use synaps_cli::extensions::runtime::{ExtensionHandler, ExtensionHealth};
use synaps_cli::extensions::runtime::process::ProcessExtension;

fn fixture_script() -> String {
    std::env::current_dir()
        .unwrap()
        .join("tests/fixtures/process_extension.py")
        .to_string_lossy()
        .to_string()
}

struct TestProcess {
    extension: ProcessExtension,
    _temp: tempfile::TempDir,
}

async fn spawn_fixture(id: &str, mode: &str) -> TestProcess {
    let temp = tempfile::tempdir().unwrap();
    let state = temp.path().join("state.log").to_string_lossy().to_string();
    let args = vec![fixture_script(), mode.to_string(), state];
    let extension = ProcessExtension::spawn(id, "python3", &args).await.unwrap();
    TestProcess {
        extension,
        _temp: temp,
    }
}

#[tokio::test]
async fn exits_before_response_then_respawns_and_retries() {
    let process = spawn_fixture("respawn-before-response", "exit_before_response").await;

    let event = HookEvent::before_tool_call("bash", serde_json::json!({"command": "echo hi"}));
    let result = process.extension.handle(&event).await;

    match result {
        HookResult::Block { reason } => assert_eq!(reason, "respawned"),
        other => panic!("expected block after retry, got {other:?}"),
    }
    assert_eq!(process.extension.health().await, ExtensionHealth::Restarting);
    process.extension.shutdown().await;
}

#[tokio::test]
async fn crashes_after_success_then_respawns_on_next_hook() {
    let process = spawn_fixture("respawn-after-success", "crash_after_success").await;

    let event = HookEvent::before_tool_call("bash", serde_json::json!({"command": "echo hi"}));
    assert!(matches!(process.extension.handle(&event).await, HookResult::Continue));

    let result = process.extension.handle(&event).await;
    match result {
        HookResult::Block { reason } => assert_eq!(reason, "after-crash-respawn"),
        other => panic!("expected block after crash respawn, got {other:?}"),
    }
    assert_eq!(process.extension.health().await, ExtensionHealth::Restarting);
    process.extension.shutdown().await;
}

#[tokio::test]
async fn restart_exhaustion_marks_failed_and_fails_open() {
    let process = spawn_fixture("restart-limit", "always_exit").await;

    let event = HookEvent::before_tool_call("bash", serde_json::json!({"command": "echo hi"}));
    for _ in 0..4 {
        assert!(matches!(process.extension.handle(&event).await, HookResult::Continue));
    }

    assert_eq!(process.extension.health().await, ExtensionHealth::Failed);
    assert_eq!(process.extension.restart_count(), 4);
    assert!(matches!(process.extension.handle(&event).await, HookResult::Continue));
    process.extension.shutdown().await;
}

#[tokio::test]
async fn restart_count_reports_transport_restarts() {
    let process = spawn_fixture("restart-count", "exit_before_response").await;

    assert_eq!(process.extension.restart_count(), 0);
    let event = HookEvent::before_tool_call("bash", serde_json::json!({"command": "echo hi"}));
    let result = process.extension.handle(&event).await;

    assert!(matches!(result, HookResult::Block { .. }));
    assert_eq!(process.extension.restart_count(), 1);
    process.extension.shutdown().await;
}
