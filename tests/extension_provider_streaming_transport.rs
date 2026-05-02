//! Integration test for bidirectional JSON-RPC transport in `ProcessExtension`.
//!
//! Verifies that JSON-RPC notifications (no `id`) emitted by the extension
//! during a request/response are dispatched to a registered notification
//! subscriber, while the response is still delivered to the caller.

use std::path::PathBuf;

use synaps_cli::extensions::runtime::process::{NotificationFrame, ProcessExtension};
use synaps_cli::extensions::runtime::ExtensionHandler;

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn process_extension_dispatches_notifications_to_subscriber() {
    let fixture = fixture_path("notify_then_respond_extension.py");
    assert!(fixture.exists(), "fixture missing: {:?}", fixture);

    let handler = ProcessExtension::spawn(
        "notify-then-respond",
        "python3",
        &[fixture.to_string_lossy().to_string()],
    )
    .await
    .expect("spawn fixture");

    handler
        .initialize_for_test(None)
        .await
        .expect("initialize fixture");

    let mut rx = handler.subscribe_notifications().await;

    let result = handler
        .call_tool("trigger", serde_json::json!({}))
        .await
        .expect("tool.call should succeed despite interleaved notifications");
    assert_eq!(result["status"], "ok");

    // Drain notifications. Two should have been delivered.
    let first = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
        .await
        .expect("first notification timeout")
        .expect("notification channel closed");
    let second = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
        .await
        .expect("second notification timeout")
        .expect("notification channel closed");

    assert_eq!(first.method, "test.notify");
    assert_eq!(first.params, serde_json::json!({"index": 0}));
    assert_eq!(second.method, "test.notify");
    assert_eq!(second.params, serde_json::json!({"index": 1}));

    // Sanity: type is publicly visible.
    let _: NotificationFrame = NotificationFrame {
        method: "x".into(),
        params: serde_json::json!({}),
    };

    handler.shutdown().await;
}
