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

    let (_sub_id, mut rx) = handler.subscribe_notifications().await;

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

/// CP-11 fix-2 (B): a hostile notification flood must be BACKPRESSURED by
/// the bounded notification queue, not retained in unbounded host memory.
///
/// The fixture emits 100 × ~32 KiB notifications before its response. With
/// a bounded queue, the reader stalls once queue + OS pipe are full, so the
/// RESPONSE cannot be read while the subscriber refuses to drain — and no
/// frame is lost once the subscriber resumes. (The pre-fix unbounded queue
/// ingested the whole flood instantly and completed the call without any
/// consumer progress.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn notification_flood_is_backpressured_not_retained() {
    let fixture = fixture_path("flood_notifications_extension.py");
    assert!(fixture.exists(), "fixture missing: {:?}", fixture);

    let handler = std::sync::Arc::new(
        ProcessExtension::spawn(
            "flood-notify",
            "python3",
            &[fixture.to_string_lossy().to_string()],
        )
        .await
        .expect("spawn fixture"),
    );
    handler
        .initialize_for_test(None)
        .await
        .expect("initialize fixture");

    let (_sub_id, mut rx) = handler.subscribe_notifications().await;

    let call_handler = std::sync::Arc::clone(&handler);
    let call = tokio::spawn(async move {
        call_handler
            .call_tool("trigger_flood", serde_json::json!({}))
            .await
    });

    // A stalled subscriber must stall the transport: the response sits
    // behind the flood, so the call CANNOT complete while nothing drains.
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    assert!(
        !call.is_finished(),
        "bounded notification handoff must backpressure the flood; an \
         unconsumed subscriber must not let the host ingest all frames"
    );

    // Resume consumption: every frame must arrive, in order, lossless.
    let mut indices = Vec::new();
    while indices.len() < 100 {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(10), rx.recv())
            .await
            .expect("flood frame timeout")
            .expect("notification channel closed early");
        if frame.method == "flood.delta" {
            indices.push(frame.params["index"].as_u64().unwrap());
        }
    }
    assert_eq!(indices, (0..100).collect::<Vec<u64>>());

    let result = tokio::time::timeout(std::time::Duration::from_secs(10), call)
        .await
        .expect("call timeout")
        .expect("join")
        .expect("tool.call succeeds after the flood is drained");
    assert_eq!(result["status"], "ok");

    handler.shutdown().await;
}
