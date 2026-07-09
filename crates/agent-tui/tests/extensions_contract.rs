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

use agent_engine::extensions::manifest::{
    ExtensionManifest, CURRENT_EXTENSION_PROTOCOL_VERSION,
};

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
        order[0].as_str().unwrap_or_default().contains("user theme TOML"),
        "user theme TOML override must be documented as winning"
    );
}

#[test]
fn hello_ext_manifest_ships_a_token_and_validates() {
    let plugin: serde_json::Value = serde_json::from_str(HELLO_EXT_PLUGIN).unwrap();
    let ext: ExtensionManifest =
        serde_json::from_value(plugin["extension"].clone())
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
    let ext: ExtensionManifest =
        serde_json::from_value(plugin["extension"].clone()).unwrap();
    assert!(
        ext.theme_tokens.is_empty(),
        "absent theme_tokens must default to empty"
    );
    ext.validate("hello-ext")
        .expect("legacy-shaped manifest must still validate");
}
