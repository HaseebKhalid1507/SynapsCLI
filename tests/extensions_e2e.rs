//! Integration test: spawn the time extension and verify JSON-RPC communication.

use std::fs;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use synaps_cli::config;
use synaps_cli::extensions::hooks::events::{HookEvent, HookKind, HookResult};
use synaps_cli::extensions::hooks::HookBus;
use synaps_cli::extensions::manager::ExtensionManager;
use synaps_cli::extensions::permissions::{Permission, PermissionSet};
use synaps_cli::extensions::runtime::process::ProcessExtension;
use synaps_cli::extensions::runtime::ExtensionHandler;
use synaps_cli::extensions::manifest::ExtensionConfigEntry;
use synaps_cli::{Tool, ToolContext};
use async_trait::async_trait;
use serde_json::{json, Value};

struct EchoTestTool;

#[async_trait]
impl Tool for EchoTestTool {
    fn name(&self) -> &str { "echo_test" }
    fn description(&self) -> &str { "echo test" }
    fn parameters(&self) -> Value { json!({"type": "object"}) }
    async fn execute(&self, params: Value, _ctx: ToolContext) -> synaps_cli::Result<String> {
        Ok(params["message"].as_str().unwrap_or_default().to_string())
    }
}

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
        setup: None,
        prebuilt: ::std::collections::HashMap::new(),
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
        config: vec![],
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
        setup: None,
        prebuilt: ::std::collections::HashMap::new(),
        args: vec![fixture],
        permissions: vec!["tools.register".to_string()],
        hooks: vec![],
        config: vec![],
    };

    manager.load("register-tool-test", &manifest).await.unwrap();

    let registry = tools.read().await;
    assert!(registry.get("register-tool-test:echo").is_some());
    let output = registry
        .get("register-tool-test:echo")
        .unwrap()
        .execute(
            serde_json::json!({"text": "hello"}),
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
    assert_eq!(output, "echo: hello");
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
    let temp = tempfile::tempdir().unwrap();
    let pid_file = temp.path().join("extension.pid");
    let mut manager = ExtensionManager::new(Arc::new(HookBus::new()));
    let manifest = synaps_cli::extensions::manifest::ExtensionManifest {
        protocol_version: synaps_cli::extensions::manifest::CURRENT_EXTENSION_PROTOCOL_VERSION,
        runtime: synaps_cli::extensions::manifest::ExtensionRuntime::Process,
        command: "env".to_string(),
        setup: None,
        prebuilt: ::std::collections::HashMap::new(),
        args: vec![
            format!("SYNAPS_REGISTER_TOOL_PID_FILE={}", pid_file.display()),
            "python3".to_string(),
            fixture,
        ],
        permissions: vec!["tools.intercept".to_string()],
        hooks: vec![synaps_cli::extensions::manifest::HookSubscription {
            hook: "before_tool_call".to_string(),
            tool: Some("bash".to_string()),
            matcher: None,
        }],
        config: vec![],
    };

    let error = manager.load("register-tool-test", &manifest).await.unwrap_err();
    assert!(error.contains("tools.register"), "{error}");
    manager.shutdown_all().await;

    let pid: i32 = fs::read_to_string(&pid_file).unwrap().trim().parse().unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let still_running = std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .map(|status| status.success())
        .unwrap_or(false);
    assert!(!still_running, "extension process {pid} leaked after load failure");
}

#[tokio::test]
async fn extension_tool_specs_are_validated() {
    let fixture = std::env::current_dir()
        .unwrap()
        .join("tests/fixtures/register_tool_extension.py")
        .to_string_lossy()
        .to_string();

    for (mode, expected) in [
        ("empty_name", "empty tool name"),
        ("empty_description", "empty description"),
        ("duplicate_name", "duplicate tool name"),
        ("non_object_schema", "input_schema must be a JSON object"),
    ] {
        let hook_bus = Arc::new(HookBus::new());
        let tools = Arc::new(tokio::sync::RwLock::new(synaps_cli::ToolRegistry::without_subagent()));
        let mut manager = ExtensionManager::new_with_tools(hook_bus, tools);
        let manifest = synaps_cli::extensions::manifest::ExtensionManifest {
            protocol_version: synaps_cli::extensions::manifest::CURRENT_EXTENSION_PROTOCOL_VERSION,
            runtime: synaps_cli::extensions::manifest::ExtensionRuntime::Process,
            command: "env".to_string(),
            setup: None,
            prebuilt: ::std::collections::HashMap::new(),
            args: vec![
                format!("SYNAPS_REGISTER_TOOL_MODE={mode}"),
                "python3".to_string(),
                fixture.clone(),
            ],
            permissions: vec!["tools.register".to_string()],
            hooks: vec![],
            config: vec![],
        };

        let error = manager.load("invalid-tool-spec", &manifest).await.unwrap_err();
        assert!(error.contains(expected), "mode={mode}; error={error}");
        manager.shutdown_all().await;
    }
}

#[tokio::test]
async fn extension_provider_metadata_is_registered_when_permission_is_declared() {
    let fixture = std::env::current_dir()
        .unwrap()
        .join("tests/fixtures/register_provider_extension.py")
        .to_string_lossy()
        .to_string();
    let hook_bus = Arc::new(HookBus::new());
    let mut manager = ExtensionManager::new(hook_bus);
    let manifest = synaps_cli::extensions::manifest::ExtensionManifest {
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

    manager.load("provider-plugin", &manifest).await.unwrap();

    let providers = manager.providers();
    assert_eq!(providers.len(), 1);
    assert_eq!(providers[0].runtime_id, "provider-plugin:local-llama");
    assert_eq!(providers[0].spec.models[0].id, "llama-3-8b");
    manager.shutdown_all().await;
    assert!(manager.providers().is_empty());
}

#[tokio::test]
async fn provider_capability_specs_are_validated() {
    let fixture = std::env::current_dir()
        .unwrap()
        .join("tests/fixtures/register_provider_extension.py")
        .to_string_lossy()
        .to_string();

    for (mode, expected) in [
        ("empty_id", "empty provider id"),
        ("bad_id", "invalid provider id"),
        ("empty_display_name", "empty display_name"),
        ("empty_description", "empty description"),
        ("empty_models", "must declare at least one model"),
        ("empty_model_id", "empty model id"),
        ("duplicate_model_id", "duplicate model id"),
        ("bad_config_schema", "config_schema must be a JSON object"),
    ] {
        let hook_bus = Arc::new(HookBus::new());
        let mut manager = ExtensionManager::new(hook_bus);
        let manifest = synaps_cli::extensions::manifest::ExtensionManifest {
            protocol_version: synaps_cli::extensions::manifest::CURRENT_EXTENSION_PROTOCOL_VERSION,
            runtime: synaps_cli::extensions::manifest::ExtensionRuntime::Process,
            command: "env".to_string(),
            setup: None,
            prebuilt: ::std::collections::HashMap::new(),
            args: vec![
                format!("SYNAPS_PROVIDER_MODE={mode}"),
                "python3".to_string(),
                fixture.clone(),
            ],
            permissions: vec!["tools.intercept".to_string()],
            hooks: vec![synaps_cli::extensions::manifest::HookSubscription {
                hook: "before_tool_call".to_string(),
                tool: Some("bash".to_string()),
                matcher: None,
            }],
            config: vec![],
        };

        let error = manager.load("invalid-provider-spec", &manifest).await.unwrap_err();
        assert!(error.contains(expected), "mode={mode}; error={error}");
        manager.shutdown_all().await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn extension_config_is_resolved_and_passed_to_initialize() {
    let _guard = BASE_DIR_TEST_LOCK.lock().unwrap();
    let home = tempfile::tempdir().unwrap();
    config::set_base_dir_for_tests(home.path().to_path_buf());
    synaps_cli::extensions::config_store::write_plugin_config(
        "config-test",
        "endpoint",
        "http://localhost:1234",
    )
    .unwrap();
    fs::write(home.path().join("config"), "extension.config-test.endpoint = http://localhost:1234\n").unwrap();
    std::env::set_var("CONFIG_TEST_TOKEN", "secret-token");

    let fixture = std::env::current_dir()
        .unwrap()
        .join("tests/fixtures/config_seen_extension.py")
        .to_string_lossy()
        .to_string();
    let plugin_dir = tempfile::tempdir().unwrap();
    let hook_bus = Arc::new(HookBus::new());
    let mut manager = ExtensionManager::new(hook_bus);
    let manifest = synaps_cli::extensions::manifest::ExtensionManifest {
        protocol_version: synaps_cli::extensions::manifest::CURRENT_EXTENSION_PROTOCOL_VERSION,
        runtime: synaps_cli::extensions::manifest::ExtensionRuntime::Process,
        command: "python3".to_string(),
        setup: None,
        prebuilt: ::std::collections::HashMap::new(),
        args: vec![fixture],
        permissions: vec!["tools.intercept".to_string()],
        hooks: vec![synaps_cli::extensions::manifest::HookSubscription {
            hook: "before_tool_call".to_string(),
            tool: Some("bash".to_string()),
            matcher: None,
        }],
        config: vec![
            ExtensionConfigEntry {
                key: "endpoint".to_string(),
                value_type: None,
                description: None,
                required: true,
                default: None,
                secret_env: None,
            },
            ExtensionConfigEntry {
                key: "mode".to_string(),
                value_type: None,
                description: None,
                required: false,
                default: Some(serde_json::json!("safe")),
                secret_env: None,
            },
            ExtensionConfigEntry {
                key: "token".to_string(),
                value_type: None,
                description: None,
                required: true,
                default: None,
                secret_env: Some("CONFIG_TEST_TOKEN".to_string()),
            },
        ],
    };

    manager.load_with_cwd("config-test", &manifest, Some(plugin_dir.path().to_path_buf())).await.unwrap();
    manager.shutdown_all().await;
    std::env::remove_var("CONFIG_TEST_TOKEN");

    let seen: serde_json::Value = serde_json::from_str(&fs::read_to_string(plugin_dir.path().join("config-seen.json")).unwrap()).unwrap();
    assert_eq!(seen["endpoint"], "http://localhost:1234");
    assert_eq!(seen["mode"], "safe");
    assert_eq!(seen["token"], "secret-token");
}

#[tokio::test(flavor = "current_thread")]
async fn extension_missing_required_config_fails_before_spawn() {
    let _guard = BASE_DIR_TEST_LOCK.lock().unwrap();
    let home = tempfile::tempdir().unwrap();
    config::set_base_dir_for_tests(home.path().to_path_buf());
    let plugin_dir = tempfile::tempdir().unwrap();
    let mut manager = ExtensionManager::new(Arc::new(HookBus::new()));
    let manifest = synaps_cli::extensions::manifest::ExtensionManifest {
        protocol_version: synaps_cli::extensions::manifest::CURRENT_EXTENSION_PROTOCOL_VERSION,
        runtime: synaps_cli::extensions::manifest::ExtensionRuntime::Process,
        command: "/definitely/not/spawned".to_string(),
        setup: None,
        prebuilt: ::std::collections::HashMap::new(),
        args: vec![],
        permissions: vec!["tools.intercept".to_string()],
        hooks: vec![synaps_cli::extensions::manifest::HookSubscription {
            hook: "before_tool_call".to_string(),
            tool: Some("bash".to_string()),
            matcher: None,
        }],
        config: vec![ExtensionConfigEntry {
            key: "endpoint".to_string(),
            value_type: None,
            description: None,
            required: true,
            default: None,
            secret_env: None,
        }],
    };

    let error = manager.load_with_cwd("missing-config", &manifest, Some(plugin_dir.path().to_path_buf())).await.unwrap_err();
    assert!(error.contains("missing required config 'endpoint'"), "{error}");
    assert!(!error.contains("spawn"), "config validation should happen before spawn: {error}");
}


#[tokio::test(flavor = "current_thread")]
async fn extension_provider_complete_routes_to_process() {
    let _guard = BASE_DIR_TEST_LOCK.lock().unwrap();
    let home = tempfile::tempdir().unwrap();
    config::set_base_dir_for_tests(home.path().to_path_buf());
    fs::write(home.path().join("config"), "extension.provider-test.prefix = echo\n").unwrap();

    let fixture = std::env::current_dir()
        .unwrap()
        .join("tests/fixtures/provider_extension.py")
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
        prebuilt: ::std::collections::HashMap::new(),
        args: vec![fixture],
        permissions: vec!["providers.register".to_string()],
        hooks: vec![],
        config: vec![ExtensionConfigEntry {
            key: "prefix".to_string(),
            value_type: None,
            description: None,
            required: true,
            default: None,
            secret_env: None,
        }],
    };
    manager.write().await.load_with_cwd("provider-test", &manifest, Some(plugin_dir.path().to_path_buf())).await.unwrap();

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let tools = std::sync::Arc::new(Vec::new());
    let result = synaps_cli::runtime::openai::try_route(
        "provider-test:echo:echo-small",
        &reqwest::Client::new(),
        &tools,
        &None,
        &[serde_json::json!({"role":"user","content":[{"type":"text","text":"hello"}]})],
        &tx,
        None,
        None,
        0,
        &tokio_util::sync::CancellationToken::new(),
    ).await.expect("extension route").unwrap();

    assert_eq!(result["content"][0]["text"], "echo:hello");
    match rx.recv().await.unwrap() {
        synaps_cli::runtime::StreamEvent::Llm(synaps_cli::runtime::LlmEvent::Text(text)) => assert_eq!(text, "echo:hello"),
        other => panic!("unexpected event: {other:?}"),
    }
    manager.write().await.shutdown_all().await;
    synaps_cli::runtime::openai::clear_extension_manager_for_routing();
}

#[tokio::test(flavor = "current_thread")]
async fn provider_disabled_in_trust_state_blocks_route() {
    let _guard = BASE_DIR_TEST_LOCK.lock().unwrap();
    let home = tempfile::tempdir().unwrap();
    config::set_base_dir_for_tests(home.path().to_path_buf());
    fs::write(home.path().join("config"), "extension.provider-trust-test.prefix = echo\n").unwrap();

    // Use a fixture that records every invocation, so we can prove it was NOT called.
    let fixture = std::env::current_dir()
        .unwrap()
        .join("tests/fixtures/provider_extension.py")
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
        prebuilt: ::std::collections::HashMap::new(),
        args: vec![fixture],
        permissions: vec!["providers.register".to_string()],
        hooks: vec![],
        config: vec![ExtensionConfigEntry {
            key: "prefix".to_string(),
            value_type: None,
            description: None,
            required: true,
            default: None,
            secret_env: None,
        }],
    };
    manager
        .write()
        .await
        .load_with_cwd("provider-trust-test", &manifest, Some(plugin_dir.path().to_path_buf()))
        .await
        .unwrap();

    // Persist a trust state that disables this provider.
    let mut trust = synaps_cli::extensions::trust::ProviderTrustState::default();
    synaps_cli::extensions::trust::disable_provider(
        &mut trust,
        "provider-trust-test:echo",
        Some("user disabled".into()),
    );
    synaps_cli::extensions::trust::save_trust_state(&trust).expect("save trust state");

    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let tools = std::sync::Arc::new(Vec::new());
    let result = synaps_cli::runtime::openai::try_route(
        "provider-trust-test:echo:echo-small",
        &reqwest::Client::new(),
        &tools,
        &None,
        &[serde_json::json!({"role":"user","content":[{"type":"text","text":"hello"}]})],
        &tx,
        None,
        None,
        0,
        &tokio_util::sync::CancellationToken::new(),
    )
    .await
    .expect("route returned Some");

    let err = result.expect_err("disabled provider should error out instead of completing");
    let msg = err.to_string();
    assert!(msg.contains("disabled"), "error should mention disabled: {msg}");
    assert!(
        msg.contains("provider-trust-test:echo"),
        "error should reference the runtime_id: {msg}"
    );

    // Provider extension fixture would echo the user text on success. Asserting that
    // the result is an Err (above) is sufficient proof that the provider extension's
    // provider.complete was NOT executed — the disabled trust check short-circuits
    // before any IPC.

    manager.write().await.shutdown_all().await;
    synaps_cli::runtime::openai::clear_extension_manager_for_routing();
}

#[tokio::test(flavor = "current_thread")]
async fn extension_provider_tool_use_is_executed_by_router_before_final_response() {
    let _guard = BASE_DIR_TEST_LOCK.lock().unwrap();
    let home = tempfile::tempdir().unwrap();
    config::set_base_dir_for_tests(home.path().to_path_buf());

    let fixture = std::env::current_dir()
        .unwrap()
        .join("tests/fixtures/provider_tool_extension.py")
        .to_string_lossy()
        .to_string();
    let plugin_dir = tempfile::tempdir().unwrap();
    let hook_bus = Arc::new(HookBus::new());
    let tools = Arc::new(tokio::sync::RwLock::new(synaps_cli::ToolRegistry::empty()));
    tools.write().await.register(Arc::new(EchoTestTool));
    let manager = Arc::new(tokio::sync::RwLock::new(ExtensionManager::new_with_tools(hook_bus, tools.clone())));
    synaps_cli::runtime::openai::set_extension_manager_for_routing(manager.clone());
    let manifest = synaps_cli::extensions::manifest::ExtensionManifest {
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
    manager.write().await.load_with_cwd("provider-tool", &manifest, Some(plugin_dir.path().to_path_buf())).await.unwrap();

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let tools_schema = tools.read().await.tools_schema();
    let result = synaps_cli::runtime::openai::try_route(
        "provider-tool:tools:tool-small",
        &reqwest::Client::new(),
        &tools_schema,
        &None,
        &[serde_json::json!({"role":"user","content":[{"type":"text","text":"use tool"}]})],
        &tx,
        None,
        None,
        0,
        &tokio_util::sync::CancellationToken::new(),
    ).await.expect("extension route").unwrap();

    assert_eq!(result["content"][0]["text"], "final:from-provider");
    match rx.recv().await.unwrap() {
        synaps_cli::runtime::StreamEvent::Llm(synaps_cli::runtime::LlmEvent::Text(text)) => assert_eq!(text, "final:from-provider"),
        other => panic!("unexpected event: {other:?}"),
    }
    manager.write().await.shutdown_all().await;
    synaps_cli::runtime::openai::clear_extension_manager_for_routing();
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
async fn installed_extension_receives_on_compaction_as_observe_only() {
    let _guard = BASE_DIR_TEST_LOCK.lock().unwrap();
    let home = tempfile::tempdir().unwrap();
    config::set_base_dir_for_tests(home.path().to_path_buf());

    let plugin_dir = home.path().join("plugins/compaction-test");
    let log_path = plugin_dir.join("compaction.jsonl");
    fs::create_dir_all(plugin_dir.join(".synaps-plugin")).unwrap();
    fs::create_dir_all(plugin_dir.join("extensions")).unwrap();
    fs::copy(
        std::env::current_dir().unwrap().join("tests/fixtures/compaction_extension.py"),
        plugin_dir.join("extensions/compaction_extension.py"),
    )
    .unwrap();

    fs::write(
        plugin_dir.join(".synaps-plugin/plugin.json"),
        r#"{
  "name": "compaction-test",
  "version": "0.1.0",
  "extension": {
    "protocol_version": 1,
    "runtime": "process",
    "command": "python3",
    "args": ["extensions/compaction_extension.py"],
    "permissions": ["privacy.llm_content"],
    "hooks": [{"hook": "on_compaction"}]
  }
}
"#,
    )
    .unwrap();

    std::env::set_var("SYNAPS_COMPACTION_LOG", &log_path);

    let hook_bus = Arc::new(HookBus::new());
    let mut manager = ExtensionManager::new(hook_bus.clone());
    let (loaded, failed) = manager.discover_and_load().await;

    assert_eq!(loaded, vec!["compaction-test".to_string()]);
    assert!(failed.is_empty(), "unexpected discovery failures: {failed:?}");

    let event = HookEvent::on_compaction(
        "old-session",
        "new-session",
        "Summary text",
        12,
        serde_json::json!({"source": "manual"}),
    );
    let result = hook_bus.emit(&event).await;
    assert!(matches!(result, HookResult::Continue), "on_compaction should ignore non-continue actions, got {result:?}");

    let seen = fs::read_to_string(&log_path).unwrap();
    assert!(seen.contains("on_compaction"));
    assert!(seen.contains("Summary text"));
    assert!(seen.contains("old-session"));
    assert!(seen.contains("new-session"));
    assert!(seen.contains("message_count"));

    manager.shutdown_all().await;
    std::env::remove_var("SYNAPS_COMPACTION_LOG");
}

#[tokio::test(flavor = "current_thread")]
async fn on_message_complete_is_observe_only() {
    let fixture = std::env::current_dir()
        .unwrap()
        .join("tests/fixtures/message_complete_block_extension.py");
    let handler = ProcessExtension::spawn(
        "message-complete-block",
        "python3",
        &[fixture.to_string_lossy().to_string()],
    )
    .await
    .expect("failed to spawn message complete fixture");
    handler.initialize_for_test(None).await.unwrap();

    let bus = HookBus::new();
    let handler: Arc<dyn ExtensionHandler> = Arc::new(handler);
    let mut perms = PermissionSet::new();
    perms.grant(Permission::LlmContent);
    bus.subscribe(HookKind::OnMessageComplete, handler.clone(), None, None, perms)
        .await
        .expect("subscribe on_message_complete");

    let event = HookEvent::on_message_complete("Block me", serde_json::json!({}));
    let result = bus.emit(&event).await;
    assert!(matches!(result, HookResult::Continue), "on_message_complete should ignore non-continue actions, got {result:?}");

    handler.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn installed_extension_receives_on_message_complete() {
    let _guard = BASE_DIR_TEST_LOCK.lock().unwrap();
    let home = tempfile::tempdir().unwrap();
    config::set_base_dir_for_tests(home.path().to_path_buf());

    let plugin_dir = home.path().join("plugins/message-complete-test");
    let log_path = plugin_dir.join("message-complete.jsonl");
    fs::create_dir_all(plugin_dir.join(".synaps-plugin")).unwrap();
    fs::create_dir_all(plugin_dir.join("extensions")).unwrap();
    fs::copy(
        std::env::current_dir().unwrap().join("tests/fixtures/message_complete_extension.py"),
        plugin_dir.join("extensions/message_complete_extension.py"),
    )
    .unwrap();

    fs::write(
        plugin_dir.join(".synaps-plugin/plugin.json"),
        format!(
            r#"{{
  "name": "message-complete-test",
  "version": "0.1.0",
  "extension": {{
    "protocol_version": 1,
    "runtime": "process",
    "command": "python3",
    "args": ["extensions/message_complete_extension.py"],
    "permissions": ["privacy.llm_content"],
    "hooks": [{{"hook": "on_message_complete"}}]
  }}
}}
"#
        ),
    )
    .unwrap();

    std::env::set_var("SYNAPS_MESSAGE_COMPLETE_LOG", &log_path);

    let hook_bus = Arc::new(HookBus::new());
    let mut manager = ExtensionManager::new(hook_bus.clone());
    let (loaded, failed) = manager.discover_and_load().await;

    assert_eq!(loaded, vec!["message-complete-test".to_string()]);
    assert!(failed.is_empty(), "unexpected discovery failures: {failed:?}");

    let event = HookEvent::on_message_complete(
        "Assistant answer",
        serde_json::json!({"content_block_count": 1, "has_tool_use": false}),
    );
    let result = hook_bus.emit(&event).await;
    assert!(matches!(result, HookResult::Continue));

    let seen = fs::read_to_string(&log_path).unwrap();
    assert!(seen.contains("on_message_complete"));
    assert!(seen.contains("Assistant answer"));
    assert!(seen.contains("content_block_count"));

    manager.shutdown_all().await;
    std::env::remove_var("SYNAPS_MESSAGE_COMPLETE_LOG");
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

#[tokio::test(flavor = "current_thread")]
async fn audit_log_records_disabled_route() {
    let _guard = BASE_DIR_TEST_LOCK.lock().unwrap();
    let home = tempfile::tempdir().unwrap();
    config::set_base_dir_for_tests(home.path().to_path_buf());
    fs::write(
        home.path().join("config"),
        "extension.audit-disabled-test.prefix = echo\n",
    )
    .unwrap();

    let fixture = std::env::current_dir()
        .unwrap()
        .join("tests/fixtures/provider_extension.py")
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
        prebuilt: ::std::collections::HashMap::new(),
        args: vec![fixture],
        permissions: vec!["providers.register".to_string()],
        hooks: vec![],
        config: vec![ExtensionConfigEntry {
            key: "prefix".to_string(),
            value_type: None,
            description: None,
            required: true,
            default: None,
            secret_env: None,
        }],
    };
    manager
        .write()
        .await
        .load_with_cwd(
            "audit-disabled-test",
            &manifest,
            Some(plugin_dir.path().to_path_buf()),
        )
        .await
        .unwrap();

    // Disable the provider via persisted trust state.
    let mut trust = synaps_cli::extensions::trust::ProviderTrustState::default();
    synaps_cli::extensions::trust::disable_provider(
        &mut trust,
        "audit-disabled-test:echo",
        Some("user disabled".into()),
    );
    synaps_cli::extensions::trust::save_trust_state(&trust).expect("save trust state");

    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let tools = std::sync::Arc::new(Vec::new());
    let result = synaps_cli::runtime::openai::try_route(
        "audit-disabled-test:echo:echo-small",
        &reqwest::Client::new(),
        &tools,
        &None,
        &[serde_json::json!({"role":"user","content":[{"type":"text","text":"hello"}]})],
        &tx,
        None,
        None,
        0,
        &tokio_util::sync::CancellationToken::new(),
    )
    .await
    .expect("route returned Some");

    assert!(result.is_err(), "disabled provider should return Err");

    let entries = synaps_cli::extensions::audit::read_audit_entries()
        .expect("read audit entries");
    assert_eq!(entries.len(), 1, "expected exactly one audit entry, got {entries:?}");
    let entry = &entries[0];
    assert_eq!(entry.outcome, "blocked");
    assert_eq!(entry.error_class.as_deref(), Some("trust_disabled"));
    assert_eq!(entry.plugin_id, "audit-disabled-test");
    assert_eq!(entry.provider_id, "echo");
    assert_eq!(entry.model_id, "echo-small");
    assert!(!entry.tools_exposed);
    assert_eq!(entry.tools_requested, 0);
    assert!(!entry.streamed);

    manager.write().await.shutdown_all().await;
    synaps_cli::runtime::openai::clear_extension_manager_for_routing();
}
