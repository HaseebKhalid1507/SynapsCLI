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
    manifest_with_perms_and_args(perms, vec![])
}

/// argv variant — the host scrubs extension envs (env_clear), so fixture
/// behavior must be parameterized via args.
fn manifest_with_perms_and_args(perms: Vec<&str>, extra_args: Vec<&str>) -> ExtensionManifest {
    let mut args = vec![fixture_path()];
    args.extend(extra_args.into_iter().map(String::from));
    ExtensionManifest {
        theme_tokens: Default::default(),
        deferred: None,
        protocol_version: CURRENT_EXTENSION_PROTOCOL_VERSION,
        runtime: ExtensionRuntime::Process,
        command: "python3".to_string(),
        setup: None,
        prebuilt: ::std::collections::HashMap::new(),
        args,
        permissions: perms.into_iter().map(String::from).collect(),
        hooks: vec![],
        config: vec![],
    }
}

#[tokio::test(flavor = "current_thread")]
#[allow(clippy::await_holding_lock)]
async fn extension_can_append_and_query_within_its_namespace() {
    let _guard = BASE_DIR_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
    let path = home.path().join("memory").join("memory-test-ext.jsonl");
    let body = fs::read_to_string(&path).expect("memory file should exist");
    let lines: Vec<&str> = body.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(lines.len(), 1, "expected exactly one record, got {body:?}");
    let rec: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(rec["namespace"], "memory-test-ext");
    assert_eq!(rec["content"], "hello memory");
    assert_eq!(rec["tags"][0], "@test");
}

#[tokio::test(flavor = "current_thread")]
#[allow(clippy::await_holding_lock)]
async fn extension_without_permission_cannot_append() {
    let _guard = BASE_DIR_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
#[allow(clippy::await_holding_lock)]
async fn extension_cannot_use_other_namespace() {
    let _guard = BASE_DIR_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let home = tempfile::tempdir().unwrap();
    config::set_base_dir_for_tests(home.path().to_path_buf());

    let mut manager = ExtensionManager::new(Arc::new(HookBus::new()));
    // --namespace=other-ext: foreign namespace via argv (env_clear-safe)
    let manifest = manifest_with_perms_and_args(
        vec!["memory.read", "memory.write"],
        vec!["--namespace=other-ext"],
    );

    let err = manager
        .load("memory-test-ext", &manifest)
        .await
        .expect_err("extension load should fail when using a foreign namespace");

    assert!(
        err.contains("namespace must equal"),
        "expected namespace error, got: {err}"
    );

    manager.shutdown_all().await;
}

/// T291 defect 3: JSON-RPC 2.0 §4 permits string request ids, and
/// `docs/extensions/protocol.md` uses them in its own worked examples
/// (`"evt-001"`). The runtime nonetheless required `id` to parse as u64 and
/// dropped every other frame at `trace` level, so an extension written
/// against the documentation blocked forever awaiting a response that was
/// never going to come — with no error anywhere at default log levels.
///
/// This is the end-to-end proof of the fix: the fixture issues its
/// `memory.append` / `memory.query` requests with string ids and refuses to
/// complete `initialize` unless both come back correlated to the exact id it
/// sent. A successful load therefore means string ids round-tripped.
///
/// The timeout is load-bearing: before the fix this test does not fail, it
/// HANGS, which is precisely the reported symptom.
#[tokio::test(flavor = "current_thread")]
#[allow(clippy::await_holding_lock)]
async fn extension_may_use_string_request_ids() {
    let _guard = BASE_DIR_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let home = tempfile::tempdir().unwrap();
    config::set_base_dir_for_tests(home.path().to_path_buf());

    std::env::remove_var("MEMORY_FIXTURE_NAMESPACE");
    std::env::remove_var("MEMORY_FIXTURE_CONTENT");
    std::env::remove_var("MEMORY_FIXTURE_TAG");

    let mut manager = ExtensionManager::new(Arc::new(HookBus::new()));
    let manifest =
        manifest_with_perms_and_args(vec!["memory.read", "memory.write"], vec!["--string-ids"]);

    let loaded = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        manager.load("memory-test-ext", &manifest),
    )
    .await
    .expect(
        "extension load hung: the runtime dropped a string-id request instead \
         of answering it (regression of #291 defect 3)",
    );

    loaded.expect("extension using string JSON-RPC ids should load and complete its inbound RPC");

    manager.shutdown_all().await;

    // The RPC really executed — not merely "did not error".
    let path = home.path().join("memory").join("memory-test-ext.jsonl");
    let body = fs::read_to_string(&path).expect("memory file should exist");
    let lines: Vec<&str> = body.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(lines.len(), 1, "expected exactly one record, got {body:?}");
    let rec: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(rec["content"], "hello memory");
}
