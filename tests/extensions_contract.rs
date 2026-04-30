//! Extension contract tests for manifest validation and event safety.

use synaps_cli::extensions::hooks::events::HookEvent;
use synaps_cli::extensions::manager::ExtensionManager;
use synaps_cli::extensions::permissions::PermissionSet;

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
        runtime: ExtensionRuntime::Process,
        command: "/definitely/not/a/real/extension-binary".to_string(),
        args: vec![],
        permissions: vec!["tools.typo".to_string()],
        hooks: vec![HookSubscription {
            hook: "before_tool_call".to_string(),
            tool: Some("bash".to_string()),
        }],
    };

    let err = manager.load("bad-ext", &manifest).await.unwrap_err();

    assert!(err.contains("Unknown extension permission: tools.typo"));
    assert_eq!(manager.count(), 0);
}
