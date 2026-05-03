//! Integration tests for extension info.get capability advertisement RPC.

use std::sync::Arc;

use synaps_cli::extensions::hooks::HookBus;
use synaps_cli::extensions::manager::ExtensionManager;
use synaps_cli::extensions::manifest::{
    ExtensionManifest, ExtensionRuntime, CURRENT_EXTENSION_PROTOCOL_VERSION,
};

fn fixture_path() -> String {
    std::env::current_dir()
        .unwrap()
        .join("tests/fixtures/info_extension.py")
        .to_string_lossy()
        .to_string()
}

fn manifest() -> ExtensionManifest {
    ExtensionManifest {
        protocol_version: CURRENT_EXTENSION_PROTOCOL_VERSION,
        runtime: ExtensionRuntime::Process,
        command: "python3".to_string(),
        args: vec![fixture_path()],
        permissions: vec!["audio.input".to_string(), "config.subscribe".to_string()],
        hooks: vec![],
        config: vec![],
    }
}

#[tokio::test(flavor = "current_thread")]
async fn manager_caches_plugin_info_after_initialize() {
    std::env::set_var("INFO_FIXTURE_DISABLE", "0");
    let mut manager = ExtensionManager::new(Arc::new(HookBus::new()));

    manager
        .load("info-test-ext", &manifest())
        .await
        .expect("extension should load and report info");

    let info = manager
        .plugin_info("info-test-ext")
        .expect("plugin info should be cached");
    assert_eq!(info.build.as_ref().unwrap().backend, "cpu");
    assert_eq!(info.build.as_ref().unwrap().features, vec!["fixture-backend"]);
    assert_eq!(info.capabilities[0].kind, "fixture");
    assert_eq!(info.models[0].id, "ggml-tiny.en.bin");
    assert!(info.models[0].installed);

    std::env::remove_var("INFO_FIXTURE_DISABLE");
    manager.shutdown_all().await;
}

#[tokio::test(flavor = "current_thread")]
async fn info_get_is_best_effort_and_missing_method_does_not_fail_load() {
    std::env::set_var("INFO_FIXTURE_DISABLE", "1");
    let mut manager = ExtensionManager::new(Arc::new(HookBus::new()));

    manager
        .load("legacy-ext", &manifest())
        .await
        .expect("extension load should tolerate missing info.get");

    assert!(manager.plugin_info("legacy-ext").is_none());

    std::env::remove_var("INFO_FIXTURE_DISABLE");
    manager.shutdown_all().await;
}
