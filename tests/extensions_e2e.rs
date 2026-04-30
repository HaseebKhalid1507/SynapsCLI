//! Integration test: spawn the time extension and verify JSON-RPC communication.

use std::fs;
use std::sync::{Arc, Mutex};

use synaps_cli::config;
use synaps_cli::extensions::hooks::events::{HookEvent, HookKind, HookResult};
use synaps_cli::extensions::hooks::HookBus;
use synaps_cli::extensions::manager::ExtensionManager;
use synaps_cli::extensions::permissions::{Permission, PermissionSet};
use synaps_cli::extensions::runtime::process::ProcessExtension;
use synaps_cli::extensions::runtime::ExtensionHandler;
use synaps_cli::Tool;

fn installed_fixture_script() -> String {
    std::env::current_dir()
        .unwrap()
        .join("tests/fixtures/installed_extension.py")
        .to_string_lossy()
        .to_string()
}

#[tokio::test]
async fn time_extension_injects_timestamp() {
    let ext_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("examples/extensions/time-ext.py");

    if !ext_path.exists() {
        eprintln!("Skipping: time-ext.py not found at {:?}", ext_path);
        return;
    }

    let handler = ProcessExtension::spawn(
        "time-ext",
        "python3",
        &[ext_path.to_string_lossy().to_string()],
    )
    .await
    .expect("Failed to spawn time extension");

    let event = HookEvent::before_message("What time is it?");
    let result = handler.handle(&event).await;

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
    perms.grant(Permission::LlmContent);

    bus.subscribe(HookKind::BeforeMessage, handler.clone(), None, None, perms)
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

    handler.shutdown().await;
}

#[tokio::test]
async fn time_extension_continues_for_non_message_hooks() {
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

    let event = HookEvent::before_tool_call("bash", serde_json::json!({"command": "ls"}));
    let result = handler.handle(&event).await;
    assert!(matches!(result, HookResult::Continue), "Expected Continue for tool hook");

    handler.shutdown().await;
}

static BASE_DIR_TEST_LOCK: Mutex<()> = Mutex::new(());

#[tokio::test(flavor = "current_thread")]
async fn modify_hook_replaces_tool_input_and_after_hook_sees_modified_input() {
    let fixture = std::env::current_dir()
        .unwrap()
        .join("tests/fixtures/modify_extension.py")
        .to_string_lossy()
        .to_string();

    let hook_bus = Arc::new(HookBus::new());
    let mut manager = ExtensionManager::new(hook_bus.clone());
    let manifest = synaps_cli::extensions::manifest::ExtensionManifest {
        protocol_version: synaps_cli::extensions::manifest::CURRENT_EXTENSION_PROTOCOL_VERSION,
        runtime: synaps_cli::extensions::manifest::ExtensionRuntime::Process,
        command: "python3".to_string(),
        args: vec![fixture],
        permissions: vec!["tools.intercept".to_string()],
        hooks: vec![
            synaps_cli::extensions::manifest::HookSubscription {
                hook: "before_tool_call".to_string(),
                tool: Some("bash".to_string()),
                matcher: None,
            },
            synaps_cli::extensions::manifest::HookSubscription {
                hook: "after_tool_call".to_string(),
                tool: Some("bash".to_string()),
                matcher: None,
            },
        ],
    };
    manager.load("modify-test", &manifest).await.unwrap();

    let before = synaps_cli::runtime::emit_before_tool_call(
        &hook_bus,
        "bash",
        None,
        serde_json::json!({"command": "rm -rf /tmp/nope"}),
    ).await;
    let decision = synaps_cli::runtime::resolve_before_tool_call_decision(
        serde_json::json!({"command": "rm -rf /tmp/nope"}),
        before,
        None,
    ).await;
    let input = match decision {
        synaps_cli::runtime::BeforeToolCallDecision::Continue { input } => input,
        synaps_cli::runtime::BeforeToolCallDecision::Block { reason } => panic!("unexpected block: {reason}"),
    };
    assert_eq!(input, serde_json::json!({"command": "printf modified"}));

    let output = synaps_cli::tools::BashTool
        .execute(
            input.clone(),
            synaps_cli::ToolContext {
                channels: synaps_cli::tools::ToolChannels { tx_delta: None, tx_events: None },
                capabilities: synaps_cli::tools::ToolCapabilities {
                    watcher_exit_path: None,
                    tool_register_tx: None,
                    session_manager: None,
                    subagent_registry: None,
                    event_queue: None,
                    secret_prompt: None,
                },
                limits: synaps_cli::tools::ToolLimits {
                    max_tool_output: 30_000,
                    bash_timeout: 30,
                    bash_max_timeout: 300,
                    subagent_timeout: 300,
                },
            },
        )
        .await
        .unwrap();
    assert_eq!(output, "modified");

    let after = synaps_cli::runtime::emit_after_tool_call(
        &hook_bus,
        "bash",
        None,
        input,
        output,
    ).await;
    assert!(matches!(after, HookResult::Continue));

    manager.shutdown_all().await;
}

#[tokio::test]
async fn malformed_modify_result_blocks_instead_of_failing_open() {
    let fixture = std::env::current_dir()
        .unwrap()
        .join("tests/fixtures/malformed_modify_extension.py")
        .to_string_lossy()
        .to_string();
    let handler = ProcessExtension::spawn("malformed-modify", "python3", &[fixture])
        .await
        .expect("Failed to spawn malformed modify extension");
    handler.initialize_for_test(None).await.unwrap();

    let event = HookEvent::before_tool_call("bash", serde_json::json!({"command": "rm -rf /"}));
    let result = handler.handle(&event).await;

    match result {
        HookResult::Block { reason } => assert!(reason.contains("malformed modify"), "{reason}"),
        other => panic!("expected malformed modify to block, got {other:?}"),
    }

    handler.shutdown().await;
}

#[tokio::test]
async fn extension_tools_are_registered_in_tool_registry() {
    let fixture = std::env::current_dir()
        .unwrap()
        .join("tests/fixtures/register_tool_extension.py")
        .to_string_lossy()
        .to_string();
    let hook_bus = Arc::new(HookBus::new());
    let tools = Arc::new(tokio::sync::RwLock::new(synaps_cli::ToolRegistry::without_subagent()));
    let mut manager = ExtensionManager::new_with_tools(hook_bus, tools.clone());
    let manifest = synaps_cli::extensions::manifest::ExtensionManifest {
        protocol_version: synaps_cli::extensions::manifest::CURRENT_EXTENSION_PROTOCOL_VERSION,
        runtime: synaps_cli::extensions::manifest::ExtensionRuntime::Process,
        command: "python3".to_string(),
        args: vec![fixture],
        permissions: vec!["tools.register".to_string()],
        hooks: vec![],
    };

    manager.load("register-tool-test", &manifest).await.unwrap();

    let registry = tools.read().await;
    assert!(registry.get("register-tool-test:echo").is_some());
    let schema = registry.tools_schema();
    let registered = schema.iter().find(|tool| tool["name"] == "register-tool-test_echo");
    assert!(registered.is_some(), "extension tool should appear in API schema: {schema:?}");
    drop(registry);

    manager.shutdown_all().await;
}

#[tokio::test]
async fn extension_registering_tools_requires_tools_register_permission() {
    let fixture = std::env::current_dir()
        .unwrap()
        .join("tests/fixtures/register_tool_extension.py")
        .to_string_lossy()
        .to_string();
    let mut manager = ExtensionManager::new(Arc::new(HookBus::new()));
    let manifest = synaps_cli::extensions::manifest::ExtensionManifest {
        protocol_version: synaps_cli::extensions::manifest::CURRENT_EXTENSION_PROTOCOL_VERSION,
        runtime: synaps_cli::extensions::manifest::ExtensionRuntime::Process,
        command: "python3".to_string(),
        args: vec![fixture],
        permissions: vec!["tools.intercept".to_string()],
        hooks: vec![synaps_cli::extensions::manifest::HookSubscription {
            hook: "before_tool_call".to_string(),
            tool: Some("bash".to_string()),
            matcher: None,
        }],
    };

    let error = manager.load("register-tool-test", &manifest).await.unwrap_err();
    assert!(error.contains("tools.register"), "{error}");
    manager.shutdown_all().await;
}

#[tokio::test(flavor = "current_thread")]
async fn installed_plugin_extension_is_discovered_loaded_fired_and_shutdown() {
    let _guard = BASE_DIR_TEST_LOCK.lock().unwrap();
    let home = tempfile::tempdir().unwrap();
    config::set_base_dir_for_tests(home.path().to_path_buf());

    let plugin_dir = home.path().join("plugins/installed-test");
    fs::create_dir_all(plugin_dir.join(".synaps-plugin")).unwrap();
    fs::create_dir_all(plugin_dir.join("extensions")).unwrap();
    fs::copy(installed_fixture_script(), plugin_dir.join("extensions/installed_extension.py")).unwrap();

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

#[tokio::test(flavor = "current_thread")]
async fn discovery_failures_include_plugin_manifest_path_reason_and_hint() {
    let _guard = BASE_DIR_TEST_LOCK.lock().unwrap();
    let home = tempfile::tempdir().unwrap();
    config::set_base_dir_for_tests(home.path().to_path_buf());

    let plugin_dir = home.path().join("plugins/bad-json");
    let manifest_path = plugin_dir.join(".synaps-plugin/plugin.json");
    fs::create_dir_all(manifest_path.parent().unwrap()).unwrap();
    fs::write(&manifest_path, "{ definitely not json").unwrap();

    let hook_bus = Arc::new(HookBus::new());
    let mut manager = ExtensionManager::new(hook_bus);
    let (_loaded, failed) = manager.discover_and_load().await;

    assert_eq!(failed.len(), 1);
    let failure = &failed[0];
    assert_eq!(failure.plugin, "bad-json");
    assert_eq!(failure.manifest_path.as_deref(), Some(manifest_path.as_path()));
    assert!(failure.reason.contains("Invalid plugin manifest JSON"), "{failure:?}");
    assert!(failure.hint.contains("plugin validate"), "{failure:?}");
}

#[tokio::test(flavor = "current_thread")]
async fn project_local_plugins_override_user_plugins_with_same_name() {
    let _guard = BASE_DIR_TEST_LOCK.lock().unwrap();
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    config::set_base_dir_for_tests(home.path().to_path_buf());
    let fixture = installed_fixture_script();
    let previous_cwd = std::env::current_dir().unwrap();
    std::env::set_current_dir(project.path()).unwrap();

    let user_plugin_dir = home.path().join("plugins/layered-test");
    fs::create_dir_all(user_plugin_dir.join(".synaps-plugin")).unwrap();
    fs::create_dir_all(user_plugin_dir.join("extensions")).unwrap();
    fs::copy(&fixture, user_plugin_dir.join("extensions/installed_extension.py")).unwrap();
    fs::write(
        user_plugin_dir.join(".synaps-plugin/plugin.json"),
        r#"{
  "name": "layered-test",
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

    let project_plugin_dir = project.path().join(".synaps/plugins/layered-test");
    fs::create_dir_all(project_plugin_dir.join(".synaps-plugin")).unwrap();
    fs::create_dir_all(project_plugin_dir.join("extensions")).unwrap();
    fs::copy(&fixture, project_plugin_dir.join("extensions/installed_extension.py")).unwrap();
    fs::write(
        project_plugin_dir.join(".synaps-plugin/plugin.json"),
        r#"{
  "name": "layered-test",
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

    assert!(failed.is_empty(), "unexpected discovery failures: {failed:?}");
    assert_eq!(loaded, vec!["layered-test".to_string()]);

    let event = HookEvent::before_tool_call("bash", serde_json::json!({"command": "echo local"}));
    assert!(matches!(hook_bus.emit(&event).await, HookResult::Block { .. }));
    assert!(project_plugin_dir.join("hook-seen.json").exists());
    assert!(!user_plugin_dir.join("hook-seen.json").exists());

    manager.shutdown_all().await;
    std::env::set_current_dir(previous_cwd).unwrap();
}
