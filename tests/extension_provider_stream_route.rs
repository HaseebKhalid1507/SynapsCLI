//! Integration: try_route uses provider.stream when the model declares streaming=true.

use std::sync::Arc;

use synaps_cli::config;
use synaps_cli::extensions::hooks::HookBus;
use synaps_cli::extensions::manager::ExtensionManager;

use serial_test::serial;

/// Load `fixture` (plus extra argv) as plugin `plugin_id` into a fresh
/// routing manager. Returns the manager for shutdown.
async fn load_streaming_plugin(
    plugin_id: &str,
    fixture_name: &str,
    extra_args: &[&str],
) -> Arc<tokio::sync::RwLock<ExtensionManager>> {
    let fixture = std::env::current_dir()
        .unwrap()
        .join("tests/fixtures")
        .join(fixture_name)
        .to_string_lossy()
        .to_string();
    let plugin_dir = tempfile::tempdir().unwrap();
    let hook_bus = Arc::new(HookBus::new());
    let manager = Arc::new(tokio::sync::RwLock::new(ExtensionManager::new(hook_bus)));
    synaps_cli::runtime::openai::set_extension_manager_for_routing(manager.clone());
    let mut args = vec![fixture];
    args.extend(extra_args.iter().map(|s| s.to_string()));
    let manifest = synaps_cli::extensions::manifest::ExtensionManifest {
        theme_tokens: Default::default(),
        deferred: None,
        protocol_version: synaps_cli::extensions::manifest::CURRENT_EXTENSION_PROTOCOL_VERSION,
        runtime: synaps_cli::extensions::manifest::ExtensionRuntime::Process,
        command: "python3".to_string(),
        setup: None,
        prebuilt: ::std::collections::HashMap::new(),
        args,
        permissions: vec!["providers.register".to_string()],
        hooks: vec![],
        config: vec![],
    };
    manager
        .write()
        .await
        .load_with_cwd(plugin_id, &manifest, Some(plugin_dir.path().to_path_buf()))
        .await
        .unwrap();
    manager
}

#[allow(clippy::too_many_arguments)]
async fn drive_route(
    model: &str,
    tx: &tokio::sync::mpsc::UnboundedSender<synaps_cli::runtime::StreamEvent>,
    cancel: &tokio_util::sync::CancellationToken,
) -> Option<std::result::Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>>> {
    let tools = std::sync::Arc::new(Vec::new());
    synaps_cli::runtime::openai::try_route(
        model,
        &reqwest::Client::new(),
        &tools,
        &None,
        &[std::sync::Arc::new(
            serde_json::json!({"role":"user","content":[{"type":"text","text":"hi"}]}),
        )],
        tx,
        None,
        None,
        0,
        synaps_cli::reasoning::ReasoningLevel::Medium,
        cancel,
        &synaps_cli::auth::CredentialSource::Local,
        &synaps_cli::auth::TokenCache::new(),
        3,
        synaps_cli::runtime::openai::catalog::ExecutionRole::Foreground,
        None,
        None,
        &synaps_cli::runtime::trace::TraceContext::disabled(),
        false,
    )
    .await
}

#[tokio::test(flavor = "current_thread")]
#[serial(extension_routing)]
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
        theme_tokens: Default::default(),
        deferred: None,
        protocol_version: synaps_cli::extensions::manifest::CURRENT_EXTENSION_PROTOCOL_VERSION,
        runtime: synaps_cli::extensions::manifest::ExtensionRuntime::Process,
        command: "python3".to_string(),
        setup: None,
        prebuilt: ::std::collections::HashMap::new(),
        args: vec![fixture],
        permissions: vec!["providers.register".to_string()],
        hooks: vec![],
        config: vec![],
    };
    manager
        .write()
        .await
        .load_with_cwd(
            "stream-test",
            &manifest,
            Some(plugin_dir.path().to_path_buf()),
        )
        .await
        .unwrap();

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let tools = std::sync::Arc::new(Vec::new());
    let result = synaps_cli::runtime::openai::try_route(
        "stream-test:stream-echo:stream-echo-mini",
        &reqwest::Client::new(),
        &tools,
        &None,
        &[std::sync::Arc::new(
            serde_json::json!({"role":"user","content":[{"type":"text","text":"hi"}]}),
        )],
        &tx,
        None,
        None,
        0,
        synaps_cli::reasoning::ReasoningLevel::Medium,
        &tokio_util::sync::CancellationToken::new(),
        &synaps_cli::auth::CredentialSource::Local,
        &synaps_cli::auth::TokenCache::new(),
        3,
        synaps_cli::runtime::openai::catalog::ExecutionRole::Foreground,
        None,
        None,
        &synaps_cli::runtime::trace::TraceContext::disabled(),
        false,
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
        if let synaps_cli::runtime::StreamEvent::Llm(synaps_cli::runtime::LlmEvent::Text(text)) =
            event
        {
            deltas.push(text);
        }
    }
    assert_eq!(deltas, vec!["hello ".to_string(), "world".to_string()]);

    manager.write().await.shutdown_all().await;
    synaps_cli::runtime::openai::clear_extension_manager_for_routing();
}

/// CP-11 fix-2 (B): a hostile extension flooding 2000 x 4 KiB TextDelta
/// events flows through the BOUNDED sidecar->notification->sink->forwarder
/// chain losslessly, and the terminal aggregated result survives.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial(extension_routing)]
async fn hostile_text_delta_flood_is_bounded_backpressured_and_lossless() {
    let home = tempfile::tempdir().unwrap();
    config::set_base_dir_for_tests(home.path().to_path_buf());
    let manager =
        load_streaming_plugin("flood-test", "flood_streaming_provider_extension.py", &[]).await;

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let cancel = tokio_util::sync::CancellationToken::new();
    // Drain concurrently (the production consumer is the stream relay).
    let collector = tokio::spawn(async move {
        let mut delta_bytes = 0usize;
        while let Some(event) = rx.recv().await {
            if let synaps_cli::runtime::StreamEvent::Llm(synaps_cli::runtime::LlmEvent::Text(
                text,
            )) = event
            {
                delta_bytes += text.len();
            }
        }
        delta_bytes
    });
    let result = drive_route("flood-test:flood:flood-mini", &tx, &cancel)
        .await
        .expect("extension route")
        .expect("provider stream succeeded");
    assert_eq!(result["content"][0]["text"], "flood-final");
    drop(tx);
    let delta_bytes = collector.await.unwrap();
    assert_eq!(
        delta_bytes,
        2000 * 4096,
        "the bounded handoff chain must deliver every hostile delta byte \
         to the boundary (backpressure, not loss)"
    );

    manager.write().await.shutdown_all().await;
    synaps_cli::runtime::openai::clear_extension_manager_for_routing();
}

/// CP-11 fix-2 (B): cancelling mid-flood releases the route promptly (the
/// forwarder is aborted and the in-flight sidecar call is dropped) even
/// though the extension NEVER stops streaming.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial(extension_routing)]
async fn hostile_flood_cancellation_releases_route_and_forwarder() {
    let home = tempfile::tempdir().unwrap();
    config::set_base_dir_for_tests(home.path().to_path_buf());
    let manager = load_streaming_plugin(
        "flood-forever",
        "flood_streaming_provider_extension.py",
        &["forever"],
    )
    .await;

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let cancel = tokio_util::sync::CancellationToken::new();
    let cancel_route = cancel.clone();
    let route = tokio::spawn(async move {
        drive_route("flood-forever:flood:flood-mini", &tx, &cancel_route).await
    });

    // Wait for proof the flood is live, then cancel.
    let first = tokio::time::timeout(std::time::Duration::from_secs(20), rx.recv())
        .await
        .expect("first flood delta must arrive");
    assert!(first.is_some());
    cancel.cancel();

    let outcome = tokio::time::timeout(std::time::Duration::from_secs(10), route)
        .await
        .expect("cancelled route must return promptly")
        .expect("join")
        .expect("the extension route must have claimed this model");
    let error = outcome.expect_err("cancellation must surface as an error");
    assert!(
        error.to_string().contains("canceled"),
        "unexpected cancel error: {error}"
    );

    manager.write().await.shutdown_all().await;
    synaps_cli::runtime::openai::clear_extension_manager_for_routing();
}
