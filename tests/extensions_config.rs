//! Integration tests for extension plugin-namespaced config RPC.

use std::sync::{Arc, Mutex};

use synaps_cli::config;
use synaps_cli::extensions::hooks::HookBus;
use synaps_cli::extensions::manager::ExtensionManager;
use synaps_cli::extensions::manifest::{
    ExtensionManifest, ExtensionRuntime, CURRENT_EXTENSION_PROTOCOL_VERSION,
};

static BASE_DIR_TEST_LOCK: Mutex<()> = Mutex::new(());

fn fixture_path() -> String {
    std::env::current_dir()
        .unwrap()
        .join("tests/fixtures/config_extension.py")
        .to_string_lossy()
        .to_string()
}

fn manifest_with_perms(perms: Vec<&str>) -> ExtensionManifest {
    ExtensionManifest {
        protocol_version: CURRENT_EXTENSION_PROTOCOL_VERSION,
        runtime: ExtensionRuntime::Process,
        command: "python3".to_string(),
        setup: None,
        prebuilt: ::std::collections::HashMap::new(),
        args: vec![fixture_path()],
        permissions: perms.into_iter().map(String::from).collect(),
        hooks: vec![],
        config: vec![],
    }
}

#[tokio::test(flavor = "current_thread")]
async fn extension_can_set_and_read_own_plugin_config_file() {
    let _guard = BASE_DIR_TEST_LOCK.lock().unwrap();
    let home = tempfile::tempdir().unwrap();
    config::set_base_dir_for_tests(home.path().to_path_buf());
    std::env::remove_var("CONFIG_FIXTURE_KEY");
    std::env::remove_var("CONFIG_FIXTURE_VALUE");
    std::env::remove_var("CONFIG_FIXTURE_SET");

    let mut manager = ExtensionManager::new(Arc::new(HookBus::new()));
    let manifest = manifest_with_perms(vec!["config.write", "config.subscribe"]);

    manager
        .load("config-test-ext", &manifest)
        .await
        .expect("extension should set/read config during initialize");

    let config_path = home.path().join("plugins").join("config-test-ext").join("config");
    let body = std::fs::read_to_string(config_path).expect("plugin config file should exist");
    assert!(body.contains("backend = cpu"), "body was: {body:?}");

    manager.shutdown_all().await;
}

#[tokio::test(flavor = "current_thread")]
async fn extension_can_read_config_without_write_permission() {
    let _guard = BASE_DIR_TEST_LOCK.lock().unwrap();
    let home = tempfile::tempdir().unwrap();
    config::set_base_dir_for_tests(home.path().to_path_buf());
    synaps_cli::extensions::config_store::write_plugin_config("config-test-ext", "backend", "auto")
        .unwrap();
    std::env::set_var("CONFIG_FIXTURE_SET", "0");

    let mut manager = ExtensionManager::new(Arc::new(HookBus::new()));
    let manifest = manifest_with_perms(vec!["config.subscribe"]);

    manager
        .load("config-test-ext", &manifest)
        .await
        .expect("config.get should not require write permission");

    std::env::remove_var("CONFIG_FIXTURE_SET");
    manager.shutdown_all().await;
}

#[tokio::test(flavor = "current_thread")]
async fn extension_without_write_permission_cannot_set_config() {
    let _guard = BASE_DIR_TEST_LOCK.lock().unwrap();
    let home = tempfile::tempdir().unwrap();
    config::set_base_dir_for_tests(home.path().to_path_buf());
    std::env::remove_var("CONFIG_FIXTURE_SET");

    let mut manager = ExtensionManager::new(Arc::new(HookBus::new()));
    let manifest = manifest_with_perms(vec!["config.subscribe"]);

    let err = manager
        .load("config-test-ext", &manifest)
        .await
        .expect_err("config.set should fail without config.write");

    assert!(
        err.contains("permission denied") && err.contains("config.write"),
        "expected permission error mentioning config.write, got: {err}"
    );

    manager.shutdown_all().await;
}
