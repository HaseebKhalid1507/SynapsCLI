//! P19.2 contracts-sync guard: `docs/extensions/contract.json` is the
//! machine-readable source of truth for the extension protocol
//! (STABILITY.md §1). This test pins the two invariants of the P19.2
//! change:
//!
//! 1. `theme_tokens` is documented in contract.json as an ADDITIVE
//!    OPTIONAL manifest field — and `extension_protocol_version` is
//!    still 1 (additive-optional changes never bump the version).
//! 2. The engine actually honors that promise: the P14.1 hello-ext demo
//!    manifest (which ships one token) validates, and the same manifest
//!    with `theme_tokens` stripped validates identically — proving
//!    pre-P19.2 extensions are unaffected.

use agent_engine::extensions::manifest::{ExtensionManifest, CURRENT_EXTENSION_PROTOCOL_VERSION};

const CONTRACT: &str = include_str!("../../../docs/extensions/contract.json");
const HELLO_EXT_PLUGIN: &str =
    include_str!("../../../examples/extensions/hello-ext/.synaps-plugin/plugin.json");

#[test]
fn contract_json_parses_and_version_is_not_bumped() {
    let contract: serde_json::Value =
        serde_json::from_str(CONTRACT).expect("contract.json must be valid JSON");
    // Additive-optional field => NO protocol bump. If this fails, someone
    // bumped the version for a compatible change (or the engine drifted).
    assert_eq!(
        contract["extension_protocol_version"],
        serde_json::json!(CURRENT_EXTENSION_PROTOCOL_VERSION),
        "contract.json extension_protocol_version must match the engine and \
         must NOT be bumped for the additive-optional theme_tokens field"
    );
    assert_eq!(contract["manifest_version"], serde_json::json!(1));
}

#[test]
fn contract_json_documents_theme_tokens_as_additive_optional() {
    let contract: serde_json::Value = serde_json::from_str(CONTRACT).unwrap();
    let tt = &contract["theme_tokens"];
    assert!(
        tt.is_object(),
        "contract.json must document the theme_tokens manifest field"
    );
    assert_eq!(tt["manifest_key"], "extension.theme_tokens");
    assert_eq!(tt["status"], "optional");
    assert_eq!(tt["theme_namespace"], "ext.<plugin-id>.<token>");
    // Override order is part of the contract: user TOML wins.
    let order = tt["override_order"]
        .as_array()
        .expect("override_order must be an array");
    assert!(
        order[0]
            .as_str()
            .unwrap_or_default()
            .contains("user theme TOML"),
        "user theme TOML override must be documented as winning"
    );
}

#[test]
fn hello_ext_manifest_ships_a_token_and_validates() {
    let plugin: serde_json::Value = serde_json::from_str(HELLO_EXT_PLUGIN).unwrap();
    let ext: ExtensionManifest = serde_json::from_value(plugin["extension"].clone())
        .expect("hello-ext extension block must deserialize");
    // The P19.2 acceptance vehicle: one declared token.
    assert_eq!(
        ext.theme_tokens.get("accent").map(String::as_str),
        Some("#22d3ee"),
        "hello-ext must declare the demo token accent=#22d3ee"
    );
    ext.validate("hello-ext")
        .expect("hello-ext manifest (with theme_tokens) must validate");
}

#[test]
fn manifest_without_theme_tokens_is_unaffected() {
    // Strip the new field from hello-ext's manifest — the resulting
    // pre-P19.2-shaped manifest must parse to an empty token map and
    // validate exactly as before. This is the backward-compat guarantee.
    let mut plugin: serde_json::Value = serde_json::from_str(HELLO_EXT_PLUGIN).unwrap();
    plugin["extension"]
        .as_object_mut()
        .unwrap()
        .remove("theme_tokens");
    let ext: ExtensionManifest = serde_json::from_value(plugin["extension"].clone()).unwrap();
    assert!(
        ext.theme_tokens.is_empty(),
        "absent theme_tokens must default to empty"
    );
    ext.validate("hello-ext")
        .expect("legacy-shaped manifest must still validate");
}

// ══════════════════════════════════════════════════════════════════════════
// T291 defect 2 — the drift check that STABILITY.md §1 already promised.
//
// STABILITY.md §1 describes contract.json as "drift-checked against the
// engine in CI". Until now the only checks in this file were the protocol
// version and the theme_tokens block, so the permission list drifted EIGHT
// entries behind the engine and `provider.stream` stayed marked "reserved"
// long after it was implemented — with CI green throughout.
//
// The tests below check both directions: the contract may not omit anything
// the engine has, and may not invent anything the engine lacks.
// ══════════════════════════════════════════════════════════════════════════

use agent_engine::extensions::hooks::events::HookKind;
use agent_engine::extensions::permissions::Permission;
use std::collections::BTreeSet;

/// contract.json `permissions[]` must equal exactly the engine's set of
/// grantable (non-reserved) permissions.
#[test]
fn contract_permissions_match_engine_exactly() {
    let contract: serde_json::Value = serde_json::from_str(CONTRACT).unwrap();

    let documented: BTreeSet<String> = contract["permissions"]
        .as_array()
        .expect("contract.permissions must be an array")
        .iter()
        .map(|v| v.as_str().expect("permission must be a string").to_string())
        .collect();

    let engine: BTreeSet<String> = Permission::ALL
        .iter()
        .filter(|p| !p.is_reserved())
        .map(|p| p.as_str().to_string())
        .collect();

    let missing: Vec<_> = engine.difference(&documented).collect();
    let invented: Vec<_> = documented.difference(&engine).collect();

    assert!(
        missing.is_empty(),
        "contract.json omits permissions the engine grants: {missing:?}"
    );
    assert!(
        invented.is_empty(),
        "contract.json documents permissions the engine does not have: {invented:?}"
    );
}

/// Reserved permissions must be listed as reserved, and must never appear in
/// the grantable list — declaring one is a hard load failure.
#[test]
fn contract_reserved_permissions_match_engine_exactly() {
    let contract: serde_json::Value = serde_json::from_str(CONTRACT).unwrap();

    let documented: BTreeSet<String> = contract["reserved_permissions"]
        .as_array()
        .expect("contract.reserved_permissions must be an array")
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();

    let engine: BTreeSet<String> = Permission::ALL
        .iter()
        .filter(|p| p.is_reserved())
        .map(|p| p.as_str().to_string())
        .collect();

    assert_eq!(
        documented, engine,
        "contract.json reserved_permissions disagree with Permission::is_reserved()"
    );

    let grantable: BTreeSet<String> = contract["permissions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert!(
        grantable.is_disjoint(&documented),
        "a reserved permission is also listed as grantable"
    );
}

/// contract.json `hooks` must document every hook kind the engine defines,
/// and for each: the required permission, whether it accepts a tool filter,
/// and its exact action list — all read from the engine, not restated.
#[test]
fn contract_hooks_match_engine_exactly() {
    let contract: serde_json::Value = serde_json::from_str(CONTRACT).unwrap();
    let hooks = contract["hooks"]
        .as_object()
        .expect("contract.hooks must be an object");

    let documented: BTreeSet<String> = hooks.keys().cloned().collect();
    let engine: BTreeSet<String> = HookKind::ALL
        .iter()
        .map(|k| k.as_str().to_string())
        .collect();
    assert_eq!(
        documented, engine,
        "contract.json hook list disagrees with HookKind::ALL"
    );

    for kind in HookKind::ALL {
        let name = kind.as_str();
        let entry = &hooks[name];

        assert_eq!(
            entry["permission"].as_str(),
            Some(kind.required_permission().as_str()),
            "hook {name}: contract permission disagrees with required_permission()"
        );

        assert_eq!(
            entry["tool_filter"].as_bool(),
            Some(kind.allows_tool_filter()),
            "hook {name}: contract tool_filter disagrees with allows_tool_filter()"
        );

        let documented_actions: Vec<String> = entry["actions"]
            .as_array()
            .unwrap_or_else(|| panic!("hook {name}: actions must be an array"))
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        let engine_actions: Vec<String> = kind
            .allowed_action_names()
            .iter()
            .map(|s| s.to_string())
            .collect();

        assert_eq!(
            documented_actions, engine_actions,
            "hook {name}: contract actions disagree with allowed_action_names()"
        );
    }
}
