//! Integration tests for the `sidecar.spawn_args` plugin self-config RPC.
//!
//! Phase 7 deassume-the-host slice F: core no longer reads
//! plugin-namespaced config. Instead it asks the plugin via
//! `sidecar.spawn_args`. This file proves the round trip end-to-end:
//! a fixture extension responds to the RPC, and the manager wrapper
//! decodes the typed `SidecarSpawnArgs`.

use std::sync::Arc;

use synaps_cli::extensions::hooks::HookBus;
use synaps_cli::extensions::manager::ExtensionManager;
use synaps_cli::extensions::manifest::{
    ExtensionManifest, ExtensionRuntime, CURRENT_EXTENSION_PROTOCOL_VERSION,
};

fn fixture_path() -> String {
    std::env::current_dir()
        .unwrap()
        .join("tests/fixtures/spawn_args_extension.py")
        .to_string_lossy()
        .to_string()
}

/// argv-based mode: host scrubs extension envs (env_clear), so fixture
/// behavior is selected via --mode=X argument.
fn manifest_with_mode(mode: Option<&str>) -> ExtensionManifest {
    let mut args = vec![fixture_path()];
    if let Some(m) = mode {
        args.push(format!("--mode={m}"));
    }
    ExtensionManifest {
        theme_tokens: Default::default(),
        protocol_version: CURRENT_EXTENSION_PROTOCOL_VERSION,
        runtime: ExtensionRuntime::Process,
        command: "python3".to_string(),
        setup: None,
        prebuilt: ::std::collections::HashMap::new(),
        args,
        // Need at least one registration permission so the extension
        // is permitted to load with no hooks. `audio.input` matches
        // what the existing `extensions_info` fixture uses.
        permissions: vec!["audio.input".to_string()],
        hooks: vec![],
        config: vec![],
    }
}

#[tokio::test(flavor = "current_thread")]
async fn manager_returns_plugin_supplied_spawn_args() {
    let mut manager = ExtensionManager::new(Arc::new(HookBus::new()));

    manager.load("spawn-args-ext", &manifest_with_mode(Some("ok"))).await.unwrap();

    let spawn_args = manager
        .sidecar_spawn_args("spawn-args-ext")
        .await
        .expect("plugin should reply with spawn args");

    assert_eq!(
        spawn_args.args,
        vec![
            "--model-path",
            "/plugin/owned/model.bin",
            "--language",
            "en"
        ]
    );
    assert_eq!(spawn_args.language.as_deref(), Some("en"));

    manager.shutdown_all().await;
}

#[tokio::test(flavor = "current_thread")]
async fn manager_propagates_method_not_found_error() {
    let mut manager = ExtensionManager::new(Arc::new(HookBus::new()));

    manager.load("legacy-ext", &manifest_with_mode(Some("missing"))).await.unwrap();

    let err = manager.sidecar_spawn_args("legacy-ext").await.unwrap_err();
    assert!(
        err.contains("method not found") || err.contains("unknown method"),
        "expected method-not-found error, got: {err}"
    );

    manager.shutdown_all().await;
}

#[tokio::test(flavor = "current_thread")]
async fn manager_rejects_invalid_response_payload() {
    let mut manager = ExtensionManager::new(Arc::new(HookBus::new()));

    manager.load("invalid-ext", &manifest_with_mode(Some("invalid"))).await.unwrap();

    let err = manager
        .sidecar_spawn_args("invalid-ext")
        .await
        .unwrap_err();
    assert!(
        err.contains("Invalid sidecar.spawn_args response"),
        "expected decode-error message, got: {err}"
    );

    manager.shutdown_all().await;
}

#[tokio::test(flavor = "current_thread")]
async fn manager_accepts_empty_object_as_defaults() {
    let mut manager = ExtensionManager::new(Arc::new(HookBus::new()));

    manager.load("min-ext", &manifest_with_mode(Some("minimal"))).await.unwrap();

    let spawn_args = manager.sidecar_spawn_args("min-ext").await.unwrap();
    assert!(spawn_args.args.is_empty());
    assert_eq!(spawn_args.language, None);

    manager.shutdown_all().await;
}

#[tokio::test(flavor = "current_thread")]
async fn manager_returns_unknown_extension_for_missing_id() {
    let manager = ExtensionManager::new(Arc::new(HookBus::new()));
    let err = manager
        .sidecar_spawn_args("does-not-exist")
        .await
        .unwrap_err();
    assert!(err.contains("unknown extension"), "got: {err}");
}
