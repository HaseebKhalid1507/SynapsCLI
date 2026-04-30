//! Extension contract tests for manifest validation and event safety.

use std::collections::HashSet;
use std::sync::Mutex;

use serde_json::Value;
use synaps_cli::extensions::hooks::events::{HookEvent, HookKind};
use synaps_cli::extensions::manager::ExtensionManager;
use synaps_cli::extensions::manifest::HookMatcher;
use synaps_cli::extensions::permissions::{Permission, PermissionSet};

const ALL_HOOK_KINDS: [HookKind; 5] = [
    HookKind::BeforeToolCall,
    HookKind::AfterToolCall,
    HookKind::BeforeMessage,
    HookKind::OnSessionStart,
    HookKind::OnSessionEnd,
];

const ALL_PERMISSIONS: [Permission; 4] = [
    Permission::ToolsIntercept,
    Permission::LlmContent,
    Permission::SessionLifecycle,
    Permission::ToolsRegister,
];

const RESERVED_PERMISSIONS: [Permission; 2] = [
    Permission::ToolsOverride,
    Permission::ProvidersRegister,
];

fn extension_contract() -> Value {
    serde_json::from_str(include_str!("../docs/extensions/contract.json"))
        .expect("docs/extensions/contract.json should be valid JSON")
}

#[test]
fn contract_json_matches_rust_hook_and_permission_catalogs() {
    let contract = extension_contract();
    let hooks = contract
        .get("hooks")
        .and_then(Value::as_object)
        .expect("contract should define hooks object");
    let permissions = contract
        .get("permissions")
        .and_then(Value::as_array)
        .expect("contract should define permissions array");

    let rust_hooks: HashSet<&'static str> = ALL_HOOK_KINDS
        .iter()
        .map(HookKind::as_str)
        .collect();
    let contract_hooks: HashSet<&str> = hooks.keys().map(String::as_str).collect();
    assert_eq!(contract_hooks, rust_hooks);

    let reserved_permissions = contract
        .get("reserved_permissions")
        .and_then(Value::as_array)
        .expect("contract should define reserved_permissions array");

    for hook in ALL_HOOK_KINDS {
        assert_eq!(HookKind::from_str(hook.as_str()), Some(hook));
        let hook_contract = hooks
            .get(hook.as_str())
            .expect("hook should be present in contract");
        let contract_permission = hook_contract
            .get("permission")
            .and_then(Value::as_str)
            .expect("hook should declare required permission");
        assert_eq!(contract_permission, hook.required_permission().as_str());
        assert_eq!(
            hook_contract.get("tool_filter").and_then(Value::as_bool),
            Some(hook.allows_tool_filter())
        );
        let contract_actions: Vec<&str> = hook_contract
            .get("actions")
            .and_then(Value::as_array)
            .expect("hook should declare actions")
            .iter()
            .map(|action| action.as_str().expect("action should be a string"))
            .collect();
        assert_eq!(contract_actions, hook.allowed_action_names());
        assert!(permissions.iter().any(|permission| {
            permission.as_str() == Some(contract_permission)
        }));
    }

    let rust_permissions: HashSet<&'static str> = ALL_PERMISSIONS
        .iter()
        .map(Permission::as_str)
        .collect();
    let contract_permissions: HashSet<&str> = permissions
        .iter()
        .map(|permission| permission.as_str().expect("permission should be a string"))
        .collect();
    assert_eq!(contract_permissions, rust_permissions);

    for permission in ALL_PERMISSIONS {
        assert_eq!(Permission::parse(permission.as_str()), Some(permission));
    }

    let rust_reserved_permissions: HashSet<&'static str> = RESERVED_PERMISSIONS
        .iter()
        .map(Permission::as_str)
        .collect();
    let contract_reserved_permissions: HashSet<&str> = reserved_permissions
        .iter()
        .map(|permission| permission.as_str().expect("permission should be a string"))
        .collect();
    assert_eq!(contract_reserved_permissions, rust_reserved_permissions);

    let matchers = contract
        .get("matchers")
        .and_then(Value::as_object)
        .expect("contract should define matchers object");
    let contract_matchers: HashSet<&str> = matchers.keys().map(String::as_str).collect();
    let rust_matchers: HashSet<&'static str> = HookMatcher::SUPPORTED_KEYS.iter().copied().collect();
    assert_eq!(contract_matchers, rust_matchers);
}

#[test]
fn permission_set_rejects_unknown_permissions() {
    let result = PermissionSet::try_from_strings(&[
        "tools.intercept".to_string(),
        "tools.typo".to_string(),
    ]);

    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .contains("Unknown extension permission: tools.typo")
    );
}

#[test]
fn after_tool_call_truncates_utf8_safely() {
    let output = format!("{}é", "a".repeat(32 * 1024 - 1));

    let event = HookEvent::after_tool_call("bash", serde_json::json!({}), output);

    let truncated = event.tool_output.expect("after_tool_call should carry output");
    assert!(truncated.contains("[truncated"));
}

#[tokio::test]
async fn manager_rejects_bad_manifest_before_spawning_process() {
    use std::sync::Arc;
    use synaps_cli::extensions::hooks::HookBus;
    use synaps_cli::extensions::manifest::{
        ExtensionManifest, ExtensionRuntime, HookSubscription,
    };

    let bus = Arc::new(HookBus::new());
    let mut manager = ExtensionManager::new(bus);
    let manifest = ExtensionManifest {
        protocol_version: 1,
        runtime: ExtensionRuntime::Process,
        command: "/definitely/not/a/real/extension-binary".to_string(),
        args: vec![],
        permissions: vec!["tools.typo".to_string()],
        hooks: vec![HookSubscription {
            hook: "before_tool_call".to_string(),
            tool: Some("bash".to_string()),
            matcher: None,
        }],
    };

    let err = manager.load("bad-ext", &manifest).await.unwrap_err();

    assert!(err.contains("Unknown extension permission: tools.typo"));
    assert_eq!(manager.count(), 0);
}

static BASE_DIR_TEST_LOCK: Mutex<()> = Mutex::new(());

#[tokio::test(flavor = "current_thread")]
async fn discovery_reports_malformed_extension_and_spawn_failures() {
    let _guard = BASE_DIR_TEST_LOCK.lock().unwrap();
    use std::fs;
    use std::sync::Arc;
    use synaps_cli::config;
    use synaps_cli::extensions::hooks::HookBus;

    let home = tempfile::tempdir().unwrap();
    config::set_base_dir_for_tests(home.path().to_path_buf());

    let malformed_manifest = home.path().join("plugins/malformed/.synaps-plugin/plugin.json");
    fs::create_dir_all(malformed_manifest.parent().unwrap()).unwrap();
    fs::write(
        &malformed_manifest,
        r#"{"extension":{"protocol_version":1,"runtime":"process","command":7}}"#,
    )
    .unwrap();

    let spawn_manifest = home.path().join("plugins/spawn-fail/.synaps-plugin/plugin.json");
    fs::create_dir_all(spawn_manifest.parent().unwrap()).unwrap();
    fs::write(
        &spawn_manifest,
        r#"{
  "extension": {
    "protocol_version": 1,
    "runtime": "process",
    "command": "/definitely/not/a/real/extension-binary",
    "permissions": ["tools.intercept"],
    "hooks": [{"hook": "before_tool_call"}]
  }
}"#,
    )
    .unwrap();

    let bus = Arc::new(HookBus::new());
    let mut manager = ExtensionManager::new(bus);
    let (_loaded, failed) = manager.discover_and_load().await;

    assert_eq!(failed.len(), 2);
    let malformed = failed.iter().find(|f| f.plugin == "malformed").unwrap();
    assert_eq!(malformed.manifest_path.as_deref(), Some(malformed_manifest.as_path()));
    assert!(malformed.reason.contains("Failed to parse extension manifest"));
    assert!(malformed.hint.contains("plugin validate"));

    let spawn = failed.iter().find(|f| f.plugin == "spawn-fail").unwrap();
    assert_eq!(spawn.manifest_path.as_deref(), Some(spawn_manifest.as_path()));
    assert!(spawn.reason.contains("Failed to spawn extension"));
    assert!(spawn.hint.contains("extension command is installed"));
}
