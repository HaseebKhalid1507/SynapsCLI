//! Deferred activation with HOST-OWNED trusted context (lifecycle
//! hardening review fix).
//!
//! Proves, against the checked-in Python extension fixture and an
//! INSTALLED-SHAPED manifest (legacy `extension.tools` +
//! `activation: "deferred"` aliases, exactly what axel-memory-manager
//! 0.1 shipped):
//!   * discovery/load registers dormant exact tools and spawns ZERO
//!     processes;
//!   * exact tool activation starts the child ONCE;
//!   * the initialize request carries the host-recorded trusted
//!     `project_root` in `params.config` (host_context source — no env
//!     forwarding, no user/model input);
//!   * the runtime schema matches the declaration (execution succeeds);
//!   * the sibling tool is never activated or called;
//!   * no hook traffic and no first-turn body ever reach the child.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use agent_engine::extensions::hooks::HookBus;
use agent_engine::extensions::lease::ExtensionLeaseCapability;
use agent_engine::extensions::lifecycle::dormant_extension_tools;
use agent_engine::extensions::manager::ExtensionManager;
use agent_engine::extensions::manifest::ExtensionManifest;
use agent_engine::tools::activation::{
    activate_exact_for_user, ExecutionGate, SessionId, SessionToolSet,
};
use agent_engine::tools::catalog::ToolId;
use agent_engine::tools::{ToolCapabilities, ToolChannels, ToolContext, ToolLimits, ToolRegistry};
use serde_json::{json, Value};

const PLUGIN: &str = "axel-shaped-plugin";

fn fixture_script() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/extension_fixture.py")
}

fn tmp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("synaps-ext-hostctx-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn search_schema() -> Value {
    json!({"type":"object","properties":{"q":{"type":"string"}},"required":["q"]})
}

fn sibling_schema() -> Value {
    json!({"type":"object","properties":{}})
}

fn legacy_tools() -> Value {
    json!([
        {"name": "memory_search", "description": "search memories", "input_schema": search_schema()},
        {"name": "memory_forget", "description": "forget one memory", "input_schema": sibling_schema()},
    ])
}

/// Installed-shaped legacy manifest: top-level `tools` + `activation:
/// "deferred"` (NO native `deferred` block) plus a host-context config
/// declaration — the exact shape an installed axel memory plugin uses.
fn legacy_manifest(spy: &Path, tools_json: &Path) -> ExtensionManifest {
    serde_json::from_value(json!({
        "runtime": "process",
        "command": "python3",
        "args": [
            fixture_script().display().to_string(),
            spy.display().to_string(),
            tools_json.display().to_string(),
            "ok",
        ],
        "permissions": ["tools.register"],
        "tools": legacy_tools(),
        "activation": "deferred",
        "config": [
            {"key": "project_root", "host_context": "project_root", "required": true}
        ]
    }))
    .unwrap()
}

fn events(spy: &Path) -> Vec<String> {
    std::fs::read_to_string(spy)
        .unwrap_or_default()
        .lines()
        .map(String::from)
        .collect()
}

fn ctx(cap: ExtensionLeaseCapability) -> ToolContext {
    ToolContext {
        channels: ToolChannels {
            tx_delta: None,
            tx_events: None,
        },
        capabilities: ToolCapabilities {
            watcher_exit_path: None,
            tool_register_tx: None,
            session_manager: None,
            subagent_registry: None,
            event_queue: None,
            delegation_parent: None,
            secret_prompt: None,
            orchestration: None,
            tool_activation: None,
            mcp_leases: None,
            extension_leases: Some(cap),
            memory_context: None,
        },
        limits: ToolLimits {
            max_tool_output: 30000,
            max_tool_buffer: 256 * 1024,
            bash_timeout: 30,
            bash_max_timeout: 300,
            subagent_timeout: 300,
        },
    }
}

#[tokio::test]
async fn legacy_alias_manifest_defers_spawns_once_and_receives_host_project_root() {
    let dir = tmp_dir("legacy");
    let spy = dir.join("spy.log");
    let tools_json = dir.join("tools.json");
    std::fs::write(&tools_json, serde_json::to_vec(&legacy_tools()).unwrap()).unwrap();
    let manifest = legacy_manifest(&spy, &tools_json);

    // Legacy aliases fold into the native deferred block at deserialize.
    let deferred = manifest.deferred.as_ref().expect("aliases fold to deferred");
    assert_eq!(deferred.tools.len(), 2);
    assert!(deferred.providers.is_empty());

    let registry = Arc::new(tokio::sync::RwLock::new(ToolRegistry::new()));
    let mut mgr = ExtensionManager::new_with_tools(Arc::new(HookBus::new()), Arc::clone(&registry));
    mgr.set_progressive_deferral(true);
    mgr.load(PLUGIN, &manifest).await.unwrap();

    // Dormant exact tools registered; ZERO processes spawned.
    assert!(mgr.is_deferred(PLUGIN), "legacy manifest must defer");
    assert!(events(&spy).is_empty(), "no process at discovery/load");
    let runtime = mgr.extension_runtime();
    assert_eq!(runtime.lease_count(), 0);

    // Exact activation of ONE tool; gate authorizes it; sibling denied.
    let sid = SessionId::parse("hostctx-legacy").unwrap();
    let reg = registry.read().await;
    let mut set = SessionToolSet::progressive_core_for_catalog(sid.clone(), reg.catalog());
    activate_exact_for_user(
        &mut set,
        reg.catalog(),
        &ToolId::extension(PLUGIN, "memory_search"),
    )
    .unwrap();
    ExecutionGate::authorize_wire_call(&reg, &set, "axel-shaped-plugin:memory_search").unwrap();
    assert!(
        ExecutionGate::authorize_wire_call(&reg, &set, "axel-shaped-plugin:memory_forget")
            .is_err(),
        "sibling must stay ungranted"
    );
    assert!(events(&spy).is_empty(), "activation alone must not spawn");

    // First leased execution: spawn ONCE, initialize ONCE, schema match
    // (a digest mismatch would fail this call closed).
    let cap = ExtensionLeaseCapability::new(sid, Arc::clone(&runtime));
    let tools = dormant_extension_tools(PLUGIN, &manifest);
    let search = tools
        .iter()
        .find(|t| t.name() == "axel-shaped-plugin:memory_search")
        .unwrap();
    let out = search
        .execute(json!({"q":"needle"}), ctx(cap.clone()))
        .await
        .unwrap();
    assert_eq!(out, "called:memory_search");
    let _ = search
        .execute(json!({"q":"again"}), ctx(cap))
        .await
        .unwrap();

    let ev = events(&spy);
    assert_eq!(ev.iter().filter(|e| *e == "spawn").count(), 1, "{ev:?}");
    assert_eq!(
        ev.iter().filter(|e| *e == "request:initialize").count(),
        1,
        "{ev:?}"
    );
    assert_eq!(ev.iter().filter(|e| *e == "call:memory_search").count(), 2);
    assert_eq!(ev.iter().filter(|e| *e == "call:memory_forget").count(), 0);
    assert!(
        !ev.iter().any(|e| e.starts_with("hook:")),
        "no hook traffic may ever reach a tool-only deferred child: {ev:?}"
    );

    // Initialize carried the HOST-recorded trusted project root in
    // params.config — the same canonical root the manager discovered —
    // and nothing else (no env forwarding, no first-turn body).
    let init: Value = serde_json::from_str(
        &std::fs::read_to_string(dir.join("spy.log.init.json")).expect("init params captured"),
    )
    .unwrap();
    let expected_root = agent_core::memory::store::ProjectScope::discover(
        &std::env::current_dir().unwrap(),
    )
    .unwrap()
    .root()
    .display()
    .to_string();
    assert_eq!(
        init.pointer("/config/project_root").and_then(Value::as_str),
        Some(expected_root.as_str()),
        "initialize must carry the host-owned trusted project root"
    );
    assert_eq!(
        init.get("config").and_then(Value::as_object).map(|o| o.len()),
        Some(1),
        "resolved config carries ONLY the declared host-context key"
    );

    runtime.terminate_all();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn unknown_activation_value_fails_closed_instead_of_eager() {
    let err = serde_json::from_value::<ExtensionManifest>(json!({
        "runtime": "process",
        "command": "true",
        "permissions": ["tools.register"],
        "tools": legacy_tools(),
        "activation": "eager-ish",
    }))
    .unwrap_err();
    assert!(
        err.to_string().contains("unsupported extension activation"),
        "{err}"
    );
}

#[tokio::test]
async fn native_deferred_plus_legacy_aliases_is_ambiguous_and_rejected() {
    let err = serde_json::from_value::<ExtensionManifest>(json!({
        "runtime": "process",
        "command": "true",
        "permissions": ["tools.register"],
        "tools": legacy_tools(),
        "activation": "deferred",
        "deferred": {"tools": []},
    }))
    .unwrap_err();
    assert!(
        err.to_string().contains("both a native 'deferred' block"),
        "{err}"
    );
}

#[tokio::test]
async fn legacy_tools_without_activation_still_fold_to_deferred() {
    // Passive declarations are deferral claims either way: a legacy
    // manifest listing tools must never silently load eager.
    let manifest: ExtensionManifest = serde_json::from_value(json!({
        "runtime": "process",
        "command": "true",
        "permissions": ["tools.register"],
        "tools": legacy_tools(),
    }))
    .unwrap();
    let deferred = manifest.deferred.expect("tools alias folds to deferred");
    assert_eq!(deferred.tools.len(), 2);
}
