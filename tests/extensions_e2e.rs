//! Integration test: spawn the time extension and verify JSON-RPC communication.

#[tokio::test]
async fn time_extension_injects_timestamp() {
    use synaps_cli::extensions::hooks::events::{HookEvent, HookKind, HookResult};
    use synaps_cli::extensions::hooks::HookBus;
    use synaps_cli::extensions::permissions::{Permission, PermissionSet};
    use synaps_cli::extensions::runtime::process::ProcessExtension;
    use synaps_cli::extensions::runtime::ExtensionHandler;
    use std::sync::Arc;

    // Find the extension script
    let ext_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("examples/extensions/time-ext.py");
    
    if !ext_path.exists() {
        eprintln!("Skipping: time-ext.py not found at {:?}", ext_path);
        return;
    }

    // Spawn the extension process
    let handler = ProcessExtension::spawn(
        "time-ext",
        "python3",
        &[ext_path.to_string_lossy().to_string()],
    )
    .await
    .expect("Failed to spawn time extension");

    // Send a before_message event directly
    let event = HookEvent::before_message("What time is it?");
    let result = handler.handle(&event).await;

    // Should return Inject with a timestamp
    match result {
        HookResult::Inject { content } => {
            assert!(
                content.contains("Current date and time"),
                "Expected timestamp in injected content, got: {}",
                content
            );
            eprintln!("✓ Extension injected: {}", content);
        }
        other => panic!("Expected Inject, got {:?}", other),
    }

    // Test with the HookBus
    let bus = HookBus::new();
    let handler: Arc<dyn ExtensionHandler> = Arc::new(
        ProcessExtension::spawn(
            "time-ext-2",
            "python3",
            &[ext_path.to_string_lossy().to_string()],
        )
        .await
        .expect("Failed to spawn second instance"),
    );

    let mut perms = PermissionSet::new();
    perms.grant(Permission::LlmContent); // before_message needs this

    bus.subscribe(HookKind::BeforeMessage, handler.clone(), None, perms)
        .await
        .expect("Failed to subscribe");

    let event = HookEvent::before_message("Hello world");
    let result = bus.emit(&event).await;

    match result {
        HookResult::Inject { content } => {
            assert!(content.contains("Current date and time"));
            eprintln!("✓ HookBus dispatched inject: {}", content);
        }
        other => panic!("Expected Inject from bus, got {:?}", other),
    }

    // Clean shutdown
    handler.shutdown().await;
}

#[tokio::test]
async fn time_extension_continues_for_non_message_hooks() {
    use synaps_cli::extensions::hooks::events::{HookEvent, HookResult};
    use synaps_cli::extensions::runtime::process::ProcessExtension;
    use synaps_cli::extensions::runtime::ExtensionHandler;

    let ext_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("examples/extensions/time-ext.py");

    if !ext_path.exists() {
        return;
    }

    let handler = ProcessExtension::spawn(
        "time-ext",
        "python3",
        &[ext_path.to_string_lossy().to_string()],
    )
    .await
    .expect("Failed to spawn");

    // Non-message hooks should get Continue
    let event = HookEvent::before_tool_call("bash", serde_json::json!({"command": "ls"}));
    let result = handler.handle(&event).await;
    assert!(matches!(result, HookResult::Continue), "Expected Continue for tool hook");

    handler.shutdown().await;
}
use std::fs;
use std::sync::Arc;

use synaps_cli::config;
use synaps_cli::extensions::hooks::events::{HookEvent, HookResult};
use synaps_cli::extensions::hooks::HookBus;
use synaps_cli::extensions::manager::ExtensionManager;

fn fixture_script() -> String {
    std::env::current_dir()
        .unwrap()
        .join("tests/fixtures/installed_extension.py")
        .to_string_lossy()
        .to_string()
}

#[tokio::test]
async fn installed_plugin_extension_is_discovered_loaded_fired_and_shutdown() {
    let home = tempfile::tempdir().unwrap();
    config::set_base_dir_for_tests(home.path().to_path_buf());

    let plugin_dir = home.path().join("plugins/installed-test");
    fs::create_dir_all(plugin_dir.join(".synaps-plugin")).unwrap();
    fs::create_dir_all(plugin_dir.join("extensions")).unwrap();
    fs::copy(fixture_script(), plugin_dir.join("extensions/installed_extension.py")).unwrap();

    fs::write(
        plugin_dir.join(".synaps-plugin/plugin.json"),
        r#"{
  "name": "installed-test",
  "version": "0.1.0",
  "extension": {
    "protocol_version": 1,
    "runtime": "process",
    "command": "python3",
    "args": ["extensions/installed_extension.py"],
    "permissions": ["tools.intercept"],
    "hooks": [{"hook": "before_tool_call", "tool": "bash"}]
  }
}
"#,
    )
    .unwrap();

    let hook_bus = Arc::new(HookBus::new());
    let mut manager = ExtensionManager::new(hook_bus.clone());
    let (loaded, failed) = manager.discover_and_load().await;

    assert_eq!(loaded, vec!["installed-test".to_string()]);
    assert!(failed.is_empty(), "unexpected discovery failures: {failed:?}");
    assert_eq!(manager.count(), 1);

    let event = HookEvent::before_tool_call("bash", serde_json::json!({"command": "echo e2e"}));
    let result = hook_bus.emit(&event).await;
    match result {
        HookResult::Block { reason } => assert_eq!(reason, "installed hook fired"),
        other => panic!("expected installed extension to block, got {other:?}"),
    }

    let seen = fs::read_to_string(plugin_dir.join("hook-seen.json")).unwrap();
    assert!(seen.contains("before_tool_call"));
    assert!(seen.contains("bash"));

    manager.shutdown_all().await;
    assert_eq!(manager.count(), 0);
}
