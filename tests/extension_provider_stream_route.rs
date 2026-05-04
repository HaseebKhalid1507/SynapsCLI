//! Integration: try_route uses provider.stream when the model declares streaming=true.

use std::sync::Arc;

use synaps_cli::config;
use synaps_cli::extensions::hooks::HookBus;
use synaps_cli::extensions::manager::ExtensionManager;

#[tokio::test(flavor = "current_thread")]
async fn try_route_streams_text_deltas_when_provider_supports_streaming() {
    let home = tempfile::tempdir().unwrap();
    config::set_base_dir_for_tests(home.path().to_path_buf());

    let fixture = std::env::current_dir()
        .unwrap()
        .join("tests/fixtures/streaming_provider_extension.py")
        .to_string_lossy()
        .to_string();
    let plugin_dir = tempfile::tempdir().unwrap();
    let hook_bus = Arc::new(HookBus::new());
    let manager = Arc::new(tokio::sync::RwLock::new(ExtensionManager::new(hook_bus)));
    synaps_cli::runtime::openai::set_extension_manager_for_routing(manager.clone());
    let manifest = synaps_cli::extensions::manifest::ExtensionManifest {
        protocol_version: synaps_cli::extensions::manifest::CURRENT_EXTENSION_PROTOCOL_VERSION,
        runtime: synaps_cli::extensions::manifest::ExtensionRuntime::Process,
        command: "python3".to_string(),
        setup: None,
        args: vec![fixture],
        permissions: vec!["providers.register".to_string()],
        hooks: vec![],
        config: vec![],
    };
    manager
        .write()
        .await
        .load_with_cwd("stream-test", &manifest, Some(plugin_dir.path().to_path_buf()))
        .await
        .unwrap();

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let tools = std::sync::Arc::new(Vec::new());
    let result = synaps_cli::runtime::openai::try_route(
        "stream-test:stream-echo:stream-echo-mini",
        &reqwest::Client::new(),
        &tools,
        &None,
        &[serde_json::json!({"role":"user","content":[{"type":"text","text":"hi"}]})],
        &tx,
        None,
        None,
        0,
        &tokio_util::sync::CancellationToken::new(),
    )
    .await
    .expect("extension route")
    .expect("provider stream succeeded");

    assert_eq!(result["content"][0]["type"], "text");
    assert_eq!(result["content"][0]["text"], "hello world");

    // Drain channel — close sender so recv() can return None at the end.
    drop(tx);
    let mut deltas: Vec<String> = Vec::new();
    while let Some(event) = rx.recv().await {
        if let synaps_cli::runtime::StreamEvent::Llm(synaps_cli::runtime::LlmEvent::Text(text)) = event {
            deltas.push(text);
        }
    }
    assert_eq!(deltas, vec!["hello ".to_string(), "world".to_string()]);

    manager.write().await.shutdown_all().await;
    synaps_cli::runtime::openai::clear_extension_manager_for_routing();
}
