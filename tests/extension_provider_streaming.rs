//! Integration test for `provider.stream` over JSON-RPC notifications.

use std::path::PathBuf;
use std::time::Duration;

use synaps_cli::extensions::runtime::process::{
    ProcessExtension, ProviderCompleteParams, ProviderStreamEvent,
};
use synaps_cli::extensions::runtime::ExtensionHandler;
use tokio::sync::mpsc;

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn sample_params() -> ProviderCompleteParams {
    ProviderCompleteParams {
        provider_id: "stream-echo".to_string(),
        model_id: "stream-echo-mini".to_string(),
        model: "stream-echo:stream-echo-mini".to_string(),
        messages: vec![serde_json::json!({
            "role": "user",
            "content": [{"type": "text", "text": "hi"}]
        })],
        system_prompt: None,
        tools: vec![],
        temperature: None,
        max_tokens: None,
        thinking_budget: 0,
    }
}

async fn spawn_fixture() -> ProcessExtension {
    let fixture = fixture_path("streaming_provider_extension.py");
    assert!(fixture.exists(), "fixture missing: {:?}", fixture);
    let handler = ProcessExtension::spawn(
        "stream-echo-ext",
        "python3",
        &[fixture.to_string_lossy().to_string()],
    )
    .await
    .expect("spawn fixture");
    handler
        .initialize_for_test(None)
        .await
        .expect("initialize fixture");
    handler
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provider_stream_forwards_events_and_returns_final_result() {
    let handler = spawn_fixture().await;
    let (tx, mut rx) = mpsc::unbounded_channel::<ProviderStreamEvent>();

    let drainer = tokio::spawn(async move {
        let mut events = Vec::new();
        while let Ok(Some(ev)) =
            tokio::time::timeout(Duration::from_secs(5), rx.recv()).await
        {
            events.push(ev);
        }
        events
    });

    let result = handler
        .provider_stream(sample_params(), tx)
        .await
        .expect("provider_stream should succeed");

    let events = drainer.await.expect("drainer task");
    assert_eq!(
        events.len(),
        4,
        "expected 4 events, got {:?}",
        events
    );
    assert_eq!(
        events[0],
        ProviderStreamEvent::TextDelta {
            text: "hello ".to_string()
        }
    );
    assert_eq!(
        events[1],
        ProviderStreamEvent::TextDelta {
            text: "world".to_string()
        }
    );
    match &events[2] {
        ProviderStreamEvent::Usage { usage } => {
            assert_eq!(usage["input_tokens"], 4);
            assert_eq!(usage["output_tokens"], 2);
        }
        other => panic!("expected Usage event, got {:?}", other),
    }
    assert_eq!(events[3], ProviderStreamEvent::Done);

    assert_eq!(
        result.content,
        vec![serde_json::json!({"type": "text", "text": "hello world"})]
    );
    assert_eq!(result.stop_reason.as_deref(), Some("end_turn"));

    handler.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provider_stream_completes_when_sink_dropped() {
    let handler = spawn_fixture().await;
    let (tx, rx) = mpsc::unbounded_channel::<ProviderStreamEvent>();
    drop(rx);

    let result = handler
        .provider_stream(sample_params(), tx)
        .await
        .expect("provider_stream should still complete after sink dropped");
    assert_eq!(
        result.content,
        vec![serde_json::json!({"type": "text", "text": "hello world"})]
    );

    handler.shutdown().await;
}
