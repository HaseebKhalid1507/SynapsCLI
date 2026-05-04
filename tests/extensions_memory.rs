//! Integration tests for the extension memory protocol
//! (`memory.append` and `memory.query`).
//!
//! These tests spawn a fixture extension that exercises the inbound RPC
//! during `initialize`. The fixture reports success/failure via the
//! initialize response, so we can assert on the manager-level outcome.

use std::fs;
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
        .join("tests/fixtures/memory_extension.py")
        .to_string_lossy()
        .to_string()
}

fn manifest_with_perms(perms: Vec<&str>) -> ExtensionManifest {
    ExtensionManifest {
        protocol_version: CURRENT_EXTENSION_PROTOCOL_VERSION,
        runtime: ExtensionRuntime::Process,
        command: "python3".to_string(),
        setup: None,
        args: vec![fixture_path()],
        permissions: perms.into_iter().map(String::from).collect(),
        hooks: vec![],
        config: vec![],
    }
}

#[tokio::test(flavor = "current_thread")]
async fn extension_can_append_and_query_within_its_namespace() {
    let _guard = BASE_DIR_TEST_LOCK.lock().unwrap();
    let home = tempfile::tempdir().unwrap();
    config::set_base_dir_for_tests(home.path().to_path_buf());

    // Make sure the fixture's namespace defaults to its extension id.
    std::env::remove_var("MEMORY_FIXTURE_NAMESPACE");
    std::env::remove_var("MEMORY_FIXTURE_CONTENT");
    std::env::remove_var("MEMORY_FIXTURE_TAG");

    let mut manager = ExtensionManager::new(Arc::new(HookBus::new()));
    let manifest = manifest_with_perms(vec!["memory.read", "memory.write"]);

    manager
        .load("memory-test-ext", &manifest)
        .await
        .expect("extension should load and complete append+query during initialize");

    manager.shutdown_all().await;

    // Verify the JSONL file exists and contains exactly one record with the
    // expected content.
    let path = home
        .path()
        .join("memory")
        .join("memory-test-ext.jsonl");
    let body = fs::read_to_string(&path).expect("memory file should exist");
    let lines: Vec<&str> = body.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(lines.len(), 1, "expected exactly one record, got {body:?}");
    let rec: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(rec["namespace"], "memory-test-ext");
    assert_eq!(rec["content"], "hello memory");
    assert_eq!(rec["tags"][0], "@test");
}

#[tokio::test(flavor = "current_thread")]
async fn extension_without_permission_cannot_append() {
    let _guard = BASE_DIR_TEST_LOCK.lock().unwrap();
    let home = tempfile::tempdir().unwrap();
    config::set_base_dir_for_tests(home.path().to_path_buf());

    std::env::remove_var("MEMORY_FIXTURE_NAMESPACE");
    std::env::remove_var("MEMORY_FIXTURE_CONTENT");
    std::env::remove_var("MEMORY_FIXTURE_TAG");

    let mut manager = ExtensionManager::new(Arc::new(HookBus::new()));
    // Only memory.read — no write permission.
    let manifest = manifest_with_perms(vec!["memory.read"]);

    let err = manager
        .load("memory-test-ext", &manifest)
        .await
        .expect_err("extension load should fail when memory.write is missing");

    assert!(
        err.contains("permission denied") && err.contains("memory.write"),
        "expected permission-denied error mentioning memory.write, got: {err}"
    );

    manager.shutdown_all().await;
}

#[tokio::test(flavor = "current_thread")]
async fn extension_cannot_use_other_namespace() {
    let _guard = BASE_DIR_TEST_LOCK.lock().unwrap();
    let home = tempfile::tempdir().unwrap();
    config::set_base_dir_for_tests(home.path().to_path_buf());

    std::env::set_var("MEMORY_FIXTURE_NAMESPACE", "other-ext");
    std::env::set_var("MEMORY_FIXTURE_CONTENT", "hello memory");
    std::env::set_var("MEMORY_FIXTURE_TAG", "@test");

    let mut manager = ExtensionManager::new(Arc::new(HookBus::new()));
    let manifest = manifest_with_perms(vec!["memory.read", "memory.write"]);

    let err = manager
        .load("memory-test-ext", &manifest)
        .await
        .expect_err("extension load should fail when using a foreign namespace");

    std::env::remove_var("MEMORY_FIXTURE_NAMESPACE");
    std::env::remove_var("MEMORY_FIXTURE_CONTENT");
    std::env::remove_var("MEMORY_FIXTURE_TAG");

    assert!(
        err.contains("namespace must equal"),
        "expected namespace error, got: {err}"
    );

    manager.shutdown_all().await;
}
